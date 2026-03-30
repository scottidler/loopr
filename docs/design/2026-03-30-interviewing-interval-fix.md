# Design Document: Interviewing State Interval Fix

**Author:** Scott Idler
**Date:** 2026-03-30
**Status:** Implemented
**Review Passes Completed:** 3/3

## Summary

The Coordinator FSM treats the `Interviewing` state as idle, sleeping 30 seconds between iterations. This causes funnel tests to time out and degrades the real user experience. The fix adds `Interviewing` to the active-interval match arm and overrides intervals in test config.

## Problem Statement

### Background

The Coordinator FSM loop (`src/agents/coordinator.rs:1229-1241`) determines how long to sleep after each iteration based on the current state. States that need quick transitions (Planning, ActivatePhase, PhaseGate) use `active_interval_secs` (default 5s). Everything else falls through to `idle_interval_secs` (default 30s).

The `Interviewing` state was added after this match arm was written and was never added to the active group.

### Problem

Two issues:

1. **Production UX** - When the Coordinator asks an interview question and the user replies instantly via `coordinator.interview_respond` IPC, the Coordinator sleeps 30 seconds before processing the reply. This makes the interview feel unresponsive.

2. **Test timeout** - `tests/funnel.rs` uses `Config::default()` (30s idle interval) with a 120s test timeout. Each interview round takes ~30s of sleeping. With 4 rounds, the test hits exactly 120s and times out.

### Goals

- `Interviewing` state uses the active interval (5s) in production
- Funnel tests override intervals to 1s for fast execution
- No behavioral changes to other FSM states

### Non-Goals

- Redesigning the FSM sleep mechanism (e.g., event-driven wakeup on IPC)
- Changing default interval values
- Modifying the test timeout

## Proposed Solution

### Overview

Two surgical changes - one in the FSM loop, one in the test harness.

### Change 1: Add Interviewing to the active-interval match arm

**File:** `src/agents/coordinator.rs:1236-1240`

Before:
```rust
match coord_state.fsm_state {
    CoordinatorFsmState::Planning
    | CoordinatorFsmState::ActivatePhase
    | CoordinatorFsmState::PhaseGate => self.config.active_interval_secs,
    _ => self.config.idle_interval_secs,
}
```

After:
```rust
match coord_state.fsm_state {
    CoordinatorFsmState::Interviewing
    | CoordinatorFsmState::Planning
    | CoordinatorFsmState::ActivatePhase
    | CoordinatorFsmState::PhaseGate => self.config.active_interval_secs,
    _ => self.config.idle_interval_secs,
}
```

**Rationale:** `Interviewing` is an active conversation with a user. The Coordinator emits questions and waits for responses via IPC. It needs to check for replies quickly, just like `Planning` needs to check for generated artifacts.

The only states that should use the idle interval are `Executing` (workers are running, Coordinator polls periodically) and `GoalComplete` (terminal).

### Change 2: Override intervals in test config

**File:** `tests/funnel.rs`, inside `DaemonHandle::spawn()`

```rust
let mut config = Config {
    daemon: DaemonConfig {
        socket_path: socket_path.clone(),
        pid_path,
    },
    project: ProjectConfig {
        repo_path,
        ..ProjectConfig::default()
    },
    ..Config::default()
};

// Accelerate FSM loop for tests
config.agents.coordinator.active_interval_secs = 1;
config.agents.coordinator.idle_interval_secs = 1;
```

**Rationale:** Even with Change 1 fixing the `Interviewing` interval from 30s to 5s, each round still takes 5s. With 4+ rounds that is 20+ seconds of pure sleeping. Setting both to 1s makes tests execute as fast as the LLM responds, keeping the test well under the 120s timeout.

### Implementation Plan

1. Apply Change 1 to `src/agents/coordinator.rs`
2. Apply Change 2 to `tests/funnel.rs`
3. Run `otto ci` to validate

## Alternatives Considered

### Alternative 1: Only fix the test config
- **Description:** Override intervals in tests, leave production code unchanged
- **Pros:** Minimal change, tests pass
- **Cons:** Real users still experience 30s delay between interview rounds
- **Why not chosen:** The production UX issue is real and trivially fixable

### Alternative 2: Event-driven wakeup
- **Description:** Replace sleep-based polling with a notify/wakeup mechanism where IPC responses immediately wake the FSM loop
- **Pros:** Zero unnecessary sleeping, optimal responsiveness
- **Cons:** Significant refactor of the FSM loop, introduces channel coordination complexity
- **Why not chosen:** Overkill for this issue. The active interval (5s or 1s in tests) is responsive enough. Can be revisited later.

### Alternative 3: Only fix the match arm
- **Description:** Add `Interviewing` to active interval, no test config changes
- **Pros:** Single change, fixes production UX
- **Cons:** Tests still sleep 5s per round (20+ seconds of dead time). Not a timeout risk but unnecessarily slow.
- **Why not chosen:** The test override is trivial and makes the test suite faster

## Technical Considerations

### Dependencies

None. Both changes are local edits to existing code.

### Performance

No performance impact. The active interval is already used by three other states. Adding `Interviewing` just corrects an oversight.

### Testing Strategy

- Existing funnel tests should pass (they currently time out)
- `otto ci` validates lint, compile, clippy, fmt, and tests

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| 5s active interval still too slow for UX | Low | Low | Users can configure `active_interval_secs` in config. Event-driven wakeup is a future option. |
| Test flakiness from 1s interval | Low | Low | 1s is still generous for IPC round-trips in a local test |

## Open Questions

None - this is a straightforward bug fix.

## References

- `src/agents/coordinator.rs:1229-1241` - FSM loop interval selection
- `src/config.rs:200-222` - CoordinatorConfig defaults
- `tests/funnel.rs:223-244` - Test config construction
- `docs/design/2026-03-29-conversational-funnel-testing.md` - Funnel test design
- `docs/design/2026-02-28-coordinator-sequencing.md` - Coordinator FSM design
