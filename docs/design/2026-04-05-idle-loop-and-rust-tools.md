# Design Document: Coordinator Idle Loop and Rust Tool Detection

**Author:** Scott Idler
**Date:** 2026-04-05
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Two compounding bugs surfaced in the `rust-version` E2E run. The Coordinator busy-polls
the LLM while waiting for Implementers, emitting identical `done` actions until the
lifeguard kills it. And Rust projects have no auto-detected tools because `detect.rs`
explicitly returns an empty preset for `Cargo.toml`, leaving Implementers unable to
verify their own changes. These are fixed by (1) replacing the Coordinator's fixed-interval
poll with an event-driven wake on the daemon bus, and (2) adding a cargo tool preset
matching the existing JS and Python treatment.

## Problem Statement

### Background

Loopr's Coordinator drives the orchestration FSM. In Brief mode it decomposes a goal,
assigns Work to Implementers, then waits for them to finish before advancing the FSM.
Between state changes it polls on a fixed interval (`active_interval_secs`, default 5s),
calling the LLM each tick to ask whether there is anything to do.

The `rust-version` E2E run exposed two failures that compounded:

**Coordinator death-loop.** With Work assigned and Implementers running, the Coordinator
correctly determined it had nothing to do and emitted `{"action": "done", "summary":
"Planning complete, ready to activate first phase"}` each iteration. After five
consecutive identical action hashes the lifeguard escalated, the Coordinator failed,
the supervisor restarted it, and the identical state produced the identical loop. This
continued until the work completed or the supervisor gave up.

**Empty Rust tool registry.** `detect.rs` returns `Vec::new()` for `Cargo.toml`.
The Implementer started with no registered tools, tried `run_tool 'test'`, received
"Tool 'test' not found. You MUST use register_tool first", and retried the same action
until the lifeguard fired. Critically: the Implementer had already correctly diagnosed
and fixed the code (replacing the broken `env!("CARGO_BIN_EXE_...")` with a
`binary_path()` helper). It just could not verify the fix. Inspection of the iter files
confirms the error feedback pipeline is intact - the LLM saw the failure, understood it,
and fixed the code. The blocking gap was purely the empty registry.

**Root cause of the empty registry.** The system prompt lists `test`, `clippy`, `fmt`,
`build` as example tool names. The LLM trusts this list and calls `run_tool 'test'`,
but these tools do not exist for Rust projects. The `register_tool` hint in the error
response is overridden by the system prompt's apparent authority. Pre-populating the
registry is the right fix; relying on the LLM to discover and register tools on every
Rust worktree is a non-deterministic tax on a deterministic problem.

### Problem

- The Coordinator burns LLM calls at 5-second intervals during waits when the answer
  is provably "nothing changed" until external Work or Bundle state transitions.
- Rust projects have no auto-detected tools, causing Implementers to fail verification
  on every Rust target regardless of implementation quality.

### Goals

- Coordinator does not invoke the LLM while waiting for Implementers - it wakes only
  when relevant state transitions occur on the daemon event bus
- Rust projects have `test`, `clippy`, `fmt`, and `build` tools pre-registered via
  `Cargo.toml` detection, identical to the JS and Python treatment
- The lifeguard's repeated-action check does not fire during a genuine coordinator idle
  (secondary benefit of event-driven yield - the LLM is simply not called)

### Non-Goals

- Changing the lifeguard algorithm itself
- Adding new FSM states or agent types
- Fixing the system prompt's static tool name examples (made unnecessary by the preset)
- Changes to Implementer, Reviewer, or Integrator run logic

## Proposed Solution

### Overview

Two changes in priority order:

1. **Rust cargo preset** in `src/tools/detect.rs` - one function and one match arm change
2. **Event-driven coordinator yield** in `src/agents/coordinator/run.rs` - replace the
   fixed `tokio::time::sleep` with a `tokio::select!` that also listens on the daemon
   event bus for Work and Bundle transitions

A third improvement - supervisor restart observability - is described separately under
Alternatives since it requires further investigation of session log path availability.

### Architecture

#### Change 1: Rust Cargo Preset

`src/tools/detect.rs` already defines `js_preset()` and `python_preset()`. Add:

```rust
fn cargo_preset() -> Vec<ToolEntry> {
    vec![
        ToolEntry {
            name: "test".into(),
            command: "cargo test".into(),
            timeout_secs: TOOL_TEST_TIMEOUT_SECS,
            worktree: true,
        },
        ToolEntry {
            name: "clippy".into(),
            command: "cargo clippy".into(),
            timeout_secs: TOOL_LINT_TIMEOUT_SECS,
            worktree: true,
        },
        ToolEntry {
            name: "fmt".into(),
            command: "cargo fmt --all".into(),
            timeout_secs: TOOL_FORMAT_TIMEOUT_SECS,
            worktree: true,
        },
        ToolEntry {
            name: "build".into(),
            command: "cargo build".into(),
            timeout_secs: TOOL_TEST_TIMEOUT_SECS,
            worktree: true,
        },
    ]
}
```

Change the match arm in `detect_project_tools`:

```rust
// Before
"Cargo.toml" => return Vec::new(),

// After
"Cargo.toml" => cargo_preset(),
```

Note: `clippy` does not use `-D warnings` in the preset. Project-specific strictness
belongs in `loopr.yml` Layer 1 config (highest priority in `resolve_tools`), not in
the detection fallback. The preset's job is to make tools callable, not to enforce
project policy.

Note: `fmt` uses `cargo fmt --all` (apply, not check). Implementers should be able to
auto-fix formatting then commit the result.

#### Change 2: Event-Driven Coordinator Yield

The coordinator's run loop in `src/agents/coordinator/run.rs` currently ends each
iteration with:

```rust
tokio::time::sleep(Duration::from_secs(interval)).await;
```

When the FSM is in a waiting state (Planning, ActivatePhase, PhaseGate) and the
`IterationOutcome` is `Done`, there is no meaningful work for the LLM until an
Implementer or Integrator changes something. Instead of sleeping for a fixed interval
and calling the LLM again regardless:

```rust
// Idle wait: sleep OR wake early on a relevant state change
let should_skip_llm = matches!(
    outcome,
    Ok(IterationOutcome::Done(_))
) && matches!(
    coord_state.fsm_state,
    CoordinatorFsmState::Planning
    | CoordinatorFsmState::ActivatePhase
    | CoordinatorFsmState::PhaseGate
);

if should_skip_llm {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
        event = event_rx.recv() => {
            match event {
                Ok(ev) if is_coordinator_wakeup(&ev) => {
                    self.ctx.info(&format!("early wake: {}", ev.event));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    self.ctx.warn(&format!("event_rx lagged {} events", n));
                }
                _ => {}
            }
        }
    }
    continue; // re-enter loop: re-read state, then call LLM only if something changed
}
```

`is_coordinator_wakeup` matches events that indicate real state change the coordinator
must respond to:

```rust
fn is_coordinator_wakeup(ev: &DaemonEvent) -> bool {
    ev.event == "transition.completed"
        || ev.event == "tick.published"
        || ev.event == "record.created"
}
```

`transition.completed` fires on every Work and Bundle status change. The coordinator
re-reads state at the start of each iteration regardless, so over-waking on unrelated
transitions (e.g. a tick internal state change) is harmless - the LLM call is still
gated by `should_skip_llm` on the next pass.

**Threading `event_rx` into the coordinator.** The `event_tx: broadcast::Sender<DaemonEvent>`
is already present in `run_agent_task` in `src/agents/executor/lifecycle.rs`. Subscribe
before constructing the coordinator:

```rust
let event_rx = event_tx.subscribe();
// pass event_rx into Coordinator::new() or Coordinator::run()
```

The coordinator already receives an `AgentContext`. Add `event_rx` as a field on
`AgentContext` or as a direct parameter to `Coordinator::run()`. The latter is simpler
and avoids widening `AgentContext`.

**Lifeguard behavior.** The lifeguard's repeated-action check fires only on LLM calls.
When the coordinator skips the LLM call during the idle wait and uses `continue`, the
lifeguard is never consulted. The death-loop is eliminated structurally, not patched.

### Data Model

No new persisted types. The event-driven yield uses the existing `DaemonEvent` broadcast
channel already present in the daemon.

### Implementation Plan

**Phase 1 - Rust cargo preset** (isolated, zero-risk)
- Add `cargo_preset()` to `src/tools/detect.rs`
- Change `"Cargo.toml" => return Vec::new()` to `"Cargo.toml" => cargo_preset()`
- Update `test_detect_rust_project_returns_empty` - it asserts empty and must be
  inverted to assert the four preset tool names

**Phase 2 - Event-driven coordinator yield**
- Subscribe `event_tx.subscribe()` in `run_agent_task` for `AgentKind::Coordinator`
- Pass `event_rx` into `Coordinator::run()` as a parameter
- Add `is_coordinator_wakeup()` predicate
- Replace the fixed sleep with the `select!` / `continue` pattern shown above
- Verify coordinator FSM tests still pass (no LLM calls affected in those paths)

## Alternatives Considered

### Alternative 1: Exempt `done` from Lifeguard Action Check

- **Description:** Add a special case in `check_action` so `done` never triggers
  the consecutive-action escalation.
- **Pros:** Single-line change, immediate fix.
- **Cons:** Symptom fix only. The coordinator still burns LLM tokens on no-op
  iterations every 5 seconds. If the LLM gets genuinely stuck emitting `done` in
  an error state, the safety net is gone.
- **Why not chosen:** Event-driven yield eliminates the loop structurally; exempting
  `done` just lets it loop silently longer.

### Alternative 2: Add a `yield` Action to the Coordinator

- **Description:** Define a new `yield` or `wait` action the coordinator emits to
  signal "I'm waiting, check me after the next state change."
- **Pros:** Semantically explicit - the action communicates intent.
- **Cons:** The LLM is still called every interval to decide to yield, burning tokens.
  Adds action surface area. The lifeguard still sees repeated `yield` actions.
- **Why not chosen:** Removing the LLM from the idle path is strictly better than
  adding a new action to the idle path.

### Alternative 3: Require Tools in `loopr.yml`

- **Description:** Rust tool configuration is the user's responsibility via `loopr.yml`
  `agents.tools:` entries. No auto-detection for Rust.
- **Pros:** Explicit. No magic.
- **Cons:** Breaks the contract established by JS and Python presets. Every E2E target
  and every user project would need boilerplate tool config. The detection layer exists
  precisely to remove this friction.
- **Why not chosen:** Inconsistent with existing behavior for other languages.

### Alternative 4: Supervisor Restart Observability (Deferred)

- **Description:** When the supervisor restarts a coordinator, write a "retired by
  supervisor" entry to the previous session's log file before dispatching the restart.
  Proposal from the post-mortem: each agent's log should be self-contained, including
  the cause of termination even when termination comes from outside the agent.
- **Pros:** Better post-mortem traceability across the supervisor-restart boundary.
  Currently you must cross-reference `supervisor.rs` log lines against coordinator
  session IDs to reconstruct why a coordinator was restarted.
- **Cons:** Requires `AgentSession` to expose its log file path to the supervisor
  (not currently available). Adds an `fs::OpenOptions::append` write in the supervisor
  before every restart.
- **Why deferred:** The Gemini Architect's review correctly noted that for the lifeguard
  case the agent already logs its own cause of death. The supervisor-restart gap is
  real but lower priority than the two structural fixes above. Revisit once Phase 1
  and Phase 2 are shipped.

## Technical Considerations

### Dependencies

- Phase 2 requires `broadcast::Receiver<DaemonEvent>` available at coordinator
  construction time. The sender is already in `run_agent_task`. No new crates needed.

### Performance

Phase 2 reduces LLM calls during idle waits from approximately `wait_time / interval`
to 1-2 per genuine state change (one on wake, possibly one to confirm the FSM advanced).
For a 60-second implementation run with 5-second polling that is roughly 12 no-op calls
eliminated per wait cycle.

### Security

None.

### Testing Strategy

- Phase 1: Rename `test_detect_rust_project_returns_empty` to
  `test_detect_rust_project_returns_cargo_preset`. Assert the four tool names and
  that each uses a `cargo` command.
- Phase 2: Unit test `is_coordinator_wakeup()` against all known event names. Add an
  integration test that verifies the coordinator does not call the LLM between Work
  assignment and Work transition to InReview. The existing FSM correctness tests are
  unaffected.

### Rollout Plan

Phase 1 first, standalone commit - zero risk, immediately improves all Rust E2E targets.
Phase 2 in a follow-up commit after Phase 1 is validated by a clean rust-version run.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `event_rx` lags under high event volume; coordinator misses a wakeup | Low | Low | The interval timeout still fires, so worst case is one extra polling cycle. Correctness is unaffected since state is re-read at iteration start. |
| Cargo preset `cargo test` slow on large projects, blocks the tool runner timeout | Low | Medium | `TOOL_TEST_TIMEOUT_SECS` is already 300s. User can override in `loopr.yml` Layer 1. |
| Coordinator wakes on every `transition.completed` including unrelated ticks | Low | Low | Extra wake calls LLM once, sees nothing changed, returns `Done`, goes back to sleep. Token cost is minimal. |
| `broadcast::Receiver` not subscribed before relevant events fire during startup race | Low | Medium | Subscribe before the agent loop starts in `run_agent_task`, not inside `Coordinator::run()`. |

## Open Questions

- [ ] `is_coordinator_wakeup` should include `record.created` - confirmed that Work
  creation fires `record.created` (not `transition.completed`) in
  `src/daemon/handlers/work.rs:227`. The coordinator needs to wake when new Work
  items are created during Brief mode decomposition, not only when they transition.
  Update the predicate to match both events.
- [ ] Confirm that `cargo fmt --all` (apply mode) is the right default for the preset,
  vs `cargo fmt --all -- --check` (check mode). Apply mode lets the Implementer
  auto-fix formatting then commit; check mode requires a separate fix step.

## References

- `src/tools/detect.rs` - project tool detection, presets
- `src/agents/coordinator/run.rs` - coordinator run loop
- `src/daemon/supervisor.rs` - coordinator restart logic
- `src/agents/lifeguard.rs` - loop detection and escalation
- `src/ipc/protocol.rs` - `DaemonEvent` constructors and event name constants
- E2E session: `~/.local/share/loopr/sessions/20260405T221932/`
