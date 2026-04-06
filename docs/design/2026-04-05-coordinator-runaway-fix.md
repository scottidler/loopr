# Design Document: Coordinator Runaway Fix

**Author:** Scott A. Idler
**Date:** 2026-04-05
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

A cascading failure causes the coordinator to spin at millions of iterations per second, producing
gigabytes of logs and exhausting the filesystem. Three independent root causes combine to produce
the failure; each must be fixed independently. A fourth phase adds a dedicated log file for the
decomposer and downgrade Lifeguard verdict logging to `trace` to improve observability.

## Problem Statement

### Background

The coordinator runs an FSM-driven loop. When it reaches the `Decomposing` state it is supposed to
sleep between polling iterations and wake early when the background decomposition task fires a
`decomposition.completed` event. The sleep is implemented with a `tokio::select!` that races a
timer against an event channel receive. This mechanism has a fatal flaw.

### Problem

Three bugs compose into a runaway:

1. **Silent persist failure** - The background decomposition task can fail to write Plan/Spec/Phase
   records into the in-memory stores without emitting `decomposition.failed`. It always emits
   `decomposition.completed` even when the writes failed. The coordinator wakes on
   `decomposition.completed`, calls `check_fsm_transition` (which requires an active Plan in the
   store), finds nothing, and stays in `Decomposing` forever.

2. **Duplicate coordinator** - When `loopr run` is called while a coordinator is already running a
   race between two concurrent `agent.start` calls can create two coordinator sessions. The
   per-type max_pool check (Gap #26) is atomic under a single write lock and should prevent this,
   but there are paths where a second coordinator can be created before the first is observable.
   Two coordinators each polling the same stores and emitting on the same broadcast channel doubles
   the channel pressure.

3. **`Lagged` cancels the sleep** - The `tokio::select!` idle-wait logic races a `sleep` future
   against `event_rx.recv()`. The broadcast channel has a finite buffer. With any significant
   channel traffic (amplified by two coordinators each emitting `emit_iteration_completed` on every
   iteration) the buffer fills, and `recv()` immediately returns `Lagged(n)`. `select!` cancels all
   other branches when any branch completes - so `Lagged` cancels the sleep. The loop restarts with
   zero delay. The next iteration emits another event and causes another `Lagged`, and so on. The
   coordinator achieves millions of no-op iterations per second.

Bug 3 alone is a spin; Bug 1 alone is a stuck coordinator; Bug 2 alone doubles the pressure.
Together they produce a disk-exhausting runaway within minutes.

### Goals

- Decomposer persist failures surface as `decomposition.failed`, not silent success.
- A second coordinator can never be spawned while one is already in a non-terminal state.
- `Lagged` events cannot cancel the idle sleep timer.
- Decomposer emits to a dedicated log file; Lifeguard verdict logging is downgraded to `trace` in
  host agent loops.

### Non-Goals

- Blanket demotion of coordinator hot-path event logs (`debug` to `trace`) - those logs are
  appropriate at their current levels; the real fix is not spinning. Note: the targeted
  `warn` to `trace` demotion of Lifeguard verdict calls in host agent loops (Phase 4) is in
  scope - it is an architecturally correct change, not a symptom-hiding demotion.
- Redesigning the broadcast channel capacity or replacing the event bus.
- Changing how Specs or Works are dispatched once decomposition completes.

## Proposed Solution

### Overview

Four phases, in priority order:

1. Make `double_write_old_records` fatal-on-failure and gate `decomposition.completed` on its success.
2. Enforce single-coordinator invariant at every `agent.start` entry point.
3. Replace the `tokio::select!` idle-wait with a `tokio::time::timeout` wrapping an event drain loop.
4. Add a dedicated log file for the decomposer using the existing `AgentLogger` pattern; downgrade
   Lifeguard verdict logging to `trace!` in host agent loops.

### Architecture

#### Phase 1 - Decomposer persist correctness

`double_write_old_records` currently:
- returns `()` (fire-and-forget)
- logs a `warn!` on any individual record failure and returns early or `continue`s
- the background task always emits `decomposition.completed` after calling it

Fix: change `double_write_old_records` to return `Result<()>`. Any internal failure is fatal and
propagates to the background task. The background task treats an `Err` return as
`decomposition.failed` and emits accordingly.

```rust
// Before
double_write_old_records(&stores_bg, &plan_doc_bg, &markdown_bg, &child_docs, &run_dir_bg);
let _ = event_tx_bg.send(DaemonEvent::new("decomposition.completed", ...));

// After
match double_write_old_records(&stores_bg, &plan_doc_bg, &markdown_bg, &child_docs, &run_dir_bg) {
    Ok(()) => {
        let _ = event_tx_bg.send(DaemonEvent::new("decomposition.completed", ...));
    }
    Err(e) => {
        warn!("doc entry (bg): persist failed: {}", e);
        let _ = event_tx_bg.send(DaemonEvent::new("decomposition.failed",
            json!({ "goal_id": goal_id_bg, "error": e.to_string() })));
    }
}
```

#### Phase 2 - Single coordinator invariant

The atomic pool check at `agent.start` (agent.rs lines 117-134) is the correct mechanism. The
coordinator-specific path is: if any coordinator session is non-terminal, reject a new `agent.start`
with `pool_exhausted`. This check must be present at every code path that can create a coordinator.

Audit: identify all callers that issue `agent.start` with `agent_type: coordinator`:
- `doc.accept` / `doc.inject` (via `accept_plan_markdown`) - calls through `dispatch()` which goes
  through the same `handle_agent_start` with the pool check. Correct.
- `coordinator.seed_manifest` - noted in a comment in `doc.rs` as an unimplemented manifest entry
  path. If/when implemented it must route through `dispatch()`.
- Manual `loopr agent start coordinator` CLI command - routes through `dispatch()`. Correct.
- Supervisor restart (supervisor.rs:125) - routes through `dispatch()`. Correct.

The `coordinator_already_running` flag returned by `doc.accept` is logged as a clear `info!`
message rather than silently succeeding.

#### Phase 3 - Idle wait robustness

Replace the `tokio::select!` pattern with a `tokio::time::timeout` wrapping an event drain loop.
The timeout is an absolute ceiling; nothing inside the loop can cancel it.

```rust
// Before
if is_idle_waiting {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
        event = event_rx.recv() => {
            match event {
                Ok(ev) if is_coordinator_wakeup(&ev) => {
                    self.ctx.info(&format!("early wake on: {}", ev.event));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    self.ctx.warn(&format!("event_rx lagged {} events, continuing", n));
                }
                _ => {}
            }
        }
    }
} else {
    tokio::time::sleep(Duration::from_secs(interval)).await;
}

// After
if is_idle_waiting {
    let _ = tokio::time::timeout(Duration::from_secs(interval), async {
        loop {
            match event_rx.recv().await {
                Ok(ev) if is_coordinator_wakeup(&ev) => {
                    self.ctx.info(&format!("early wake on: {}", ev.event));
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    log::debug!("event_rx lagged {} events during idle wait", n);
                }
                _ => {}
            }
        }
    })
    .await
    .ok();
} else {
    tokio::time::sleep(Duration::from_secs(interval)).await;
}
```

`Lagged` no longer cancels the sleep. It drains the channel and continues the inner loop while the
outer timeout counts down. An early wakeup still works because breaking the inner loop lets the
future resolve, which the `timeout` returns as `Ok(())`.

#### Phase 4 - Decomposer and Lifeguard log files

Two components currently roll their logs into the main daemon log:

- **Decomposer** - a module (not an agent) invoked as a background task from the daemon. Its logs
  currently go to `loopr.log`. Given its background-task nature, it should write to a dedicated
  file: `{session_dir}/agents/decomposer-{run_id}.log` where `run_id` is the goal ID. This uses
  `AgentLogger::for_component("decomposer", &goal_id, session_dir)`, a new constructor added to
  `AgentLogger` that accepts a string name instead of `AgentKind`.

- **Lifeguard** - a pure state tracker with no logging of its own. The actual logging occurs in
  the host agents (e.g. `self.ctx.warn(&format!("lifeguard: {}", reason))`), which writes to
  both the agent log and `loopr.log`. Passing `AgentLogger` into `Lifeguard::new()` would
  pollute a pure logic component with I/O side effects and is rejected. Instead, host agents
  downgrade their Lifeguard verdict log calls from `warn` to `trace`. This keeps the output
  out of `loopr.log` without altering the structure of the `Lifeguard` struct.

The `AgentLogger` type (from `agent_logger.rs`) is used only for the decomposer. The decomposer
background task receives a logger constructed from the goal_id at spawn time.

### Data Model

No schema changes. `double_write_old_records` signature change is internal.

### API Design

No IPC changes. `decomposition.failed` already exists and is already a coordinator wakeup event.

### Implementation Plan

**Phase 1 - Decomposer persist correctness**
1. Change `double_write_old_records` → `Result<()>`; all warn-and-return paths become `bail!`
2. Update the background task in `accept_plan_markdown` to branch on the return value
3. `persist_doc` loop for child Docs already logs and continues on failure - this is acceptable
   because child Doc failures do not affect the Plan/Spec/Phase records the FSM reads. Leave it.
4. Add a unit test: verify that a persist failure emits `decomposition.failed` not `decomposition.completed`

**Phase 2 - Single coordinator invariant**
1. Audit all `agent.start coordinator` call sites to confirm they all route through `dispatch()`
2. Add a targeted unit test: two sequential `doc.accept` calls against the same stores assert
   that exactly one coordinator session is created (pool check holds under contention)
3. Log a clear `info!` when `coordinator_already_running` is true in `accept_plan_markdown`

**Phase 3 - Idle wait robustness**
1. Replace the `tokio::select!` block in `run.rs` with the `tokio::time::timeout` pattern
2. The async block captures `event_rx` by `&mut` reference - structured so the borrow checker
   is satisfied. The `self.ctx.info(...)` call inside the block is accessible.
3. Add unit tests: assert that Lagged does not shorten the wait window, and that a wakeup event
   causes early break
4. The `warn!` on Lagged becomes `debug!` since in the new pattern it is expected and benign

**Phase 4 - Decomposer and Lifeguard log files**
1. Add `AgentLogger::for_component(name: &str, id: &str, session_dir: Option<&Path>)` to
   `agent_logger.rs`; refactor the struct's `agent_type: AgentKind` field to `component: String`
2. Thread an `AgentLogger::for_component("decomposer", &goal_id, ...)` into the decomposer
   background task in `accept_plan_markdown`
3. In each host agent run loop, downgrade `warn!`/`self.ctx.warn(...)` Lifeguard verdict calls
   to `trace!`/`self.ctx.trace(...)` so they no longer appear in `loopr.log`

## Alternatives Considered

### Alternative 1: Demote hot-path logs to `trace`
- **Description:** Drop `iteration N`, `idle (FSM: ...)`, `event_rx lagged`, `check_fsm_transition`,
  and `sweep_integrated_to_done` to `trace` level so the file appender ignores them.
- **Pros:** Simple, minimal code change. Stops the disk explosion immediately.
- **Cons:** Bandage only. The spin still happens, still exhausts CPU. Hides a real problem.
  Traces are still written in development; the issue only disappears in production. The logs at
  their current levels are appropriate - they should not fire millions of times per second.
- **Why not chosen:** The cause of the spin is the `Lagged`-cancels-sleep bug. Fix that, and
  these logs fire at reasonable rates. Demoting them papers over the symptom.

### Alternative 2: Increase broadcast channel capacity
- **Description:** Raise the channel capacity so `Lagged` does not occur.
- **Pros:** Quick config change.
- **Cons:** Does not fix the spin - a large enough channel just delays it. With two coordinators
  emitting on every tight-loop iteration, any finite capacity will be exceeded eventually. The
  channel pressure symptom disappears only when Phase 2 (single coordinator) and Phase 3 (robust
  sleep) are both implemented.
- **Why not chosen:** Treating the symptom. The correct fix is Phase 3.

### Alternative 3: `tokio::pin!` with loop over `select!`
- **Description:** Pin the sleep future outside the select loop and re-enter select with the same
  pinned future after each Lagged event, preserving the countdown.
- **Pros:** Correct; the sleep is never re-created so the deadline holds.
- **Cons:** More verbose and less idiomatic than the timeout wrapping. The pin must be declared
  before the loop and carefully managed. `tokio::time::timeout` expresses the same intent more
  cleanly.
- **Why not chosen:** The `timeout` pattern was identified as more idiomatic by architectural
  review. Both are correct; timeout wins on readability.

## Technical Considerations

### Dependencies

No new crates. All fixes use existing tokio primitives and the existing `AgentLogger` type.

### Performance

Phase 3 directly fixes the CPU runaway. Phases 1 and 2 eliminate the conditions that escalate
the spin into a catastrophe.

### Security

No security implications.

### Testing Strategy

- Phase 1: unit test for persist failure path in `double_write_old_records` returns `Err`
- Phase 2: unit test for sequential `agent.start` - exactly one coordinator session created
- Phase 3: unit test for idle wait under channel flood - sleep completes at full interval;
  unit test for early wakeup on `decomposition.completed` event
- Phase 4: `AgentLogger::for_component` is exercised by the decomposer; Lifeguard trace
  downgrade verified by `cargo clippy` (no warn calls remain in hot paths)

### Rollout Plan

Phases execute in priority order: 1+2 → 3 → 4. Each phase is a single commit. `otto ci` must
pass between phases.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `double_write_old_records` failing in prod was silently swallowed - making it fatal could surface real errors | Med | Med | Monitor `decomposition.failed` events; add retry logic later if needed |
| Phase 3 timeout pattern breaks early-wakeup behavior | Low | High | Unit test that a `decomposition.completed` event during the wait causes an early break |
| Phase 2 audit finds a coordinator created outside `dispatch()` | Low | High | Any such path must be converted to go through `dispatch()` before Phase 2 is closed |

## Open Questions

- [ ] `coordinator.seed_manifest` does not exist in the codebase yet (only referenced in a doc
      comment). When implemented, it must route through `dispatch()`.

## References

- `src/daemon/handlers/doc.rs` - `accept_plan_markdown`, `double_write_old_records`
- `src/agents/coordinator/run.rs` - `run_fsm_loop`, idle-wait `tokio::select!`
- `src/agents/coordinator.rs` - `check_fsm_transition`
- `src/daemon/handlers/agent.rs` - `handle_agent_start`, max_pool atomic check (Gap #26)
- `src/daemon/supervisor.rs` - coordinator restart logic
- `src/agents/agent_logger.rs` - existing per-agent log file pattern
