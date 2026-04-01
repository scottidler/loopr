# Design Document: Action Batch Error Semantics

**Author:** Scott Idler + Claude
**Date:** 2026-04-01
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

When an LLM returns multiple actions in a single response (a "batch"), the action dispatch loop processes them sequentially but does not abort on `ActionError`. This allows terminal actions like `done` to fire after a failed `propose_bundle`, ending the agent session before the error can feed back to the LLM for self-correction. The fix: break the batch on `ActionError` so the error summary appears in the next iteration's context.

## Problem Statement

### Background

Agent action loops (implementer, coordinator, researcher) parse LLM responses into a list of `AgentAction` values and execute them sequentially. Each action returns an `ActionResult`. Terminal results (`Done`, `NeedHelp`, `BundleProposed`) break out of the loop immediately. Non-terminal results (`ActionError`, `ToolRun`, `FileWritten`, etc.) fall through to the next action in the batch.

The self-correction mechanism depends on errors appearing in the iteration summary, which the LLM sees on its next iteration. This works when `ActionError` is the last (or only) action in a batch - the error summary gets appended to `summaries`, the iteration returns `Continue`, and the next iteration sees it.

### Problem

LLMs frequently batch `propose_bundle` with `done` in a single response:

```json
[
  {"action": "propose_bundle", "noop_reason": "already done"},
  {"action": "done", "summary": "work complete"}
]
```

When `propose_bundle` fails (e.g., noop rejected because worktree is dirty), the `ActionError` falls through (`_ => {}` at implementer.rs:413), and `done` fires next, returning `IterationOutcome::Done`. The session exits `Ok(())`. The error message ("Use `commit` first, then propose a normal bundle") never reaches the LLM.

**Observed in E2E:** The lua-todo implementer writes `todo.lua`, proposes a noop, gets rejected, `done` fires, session ends. Handback moves work to Blocked. Coordinator retries. New implementer writes `todo.lua` again, same mistake, same failure. Infinite Ready -> InProgress -> Blocked loop.

### Goals

- `ActionError` from any action in a batch must prevent `done` from terminating the session
- Error messages must feed back to the LLM so it can self-correct on the next iteration
- The fix must apply consistently across agents that loop over action batches (implementer, coordinator, researcher). The reviewer is single-action and not affected.

### Non-Goals

- Changing the noop bundle guard logic (that's working correctly)
- Changing the `determine_work_handback` logic (already fixed in v0.1.49)
- Making the LLM "smarter" about committing (the system should be robust to this class of LLM mistake)
- Adding retry logic within a single batch (the existing iteration loop already handles retries)

## Proposed Solution

### Overview

Break the action batch on `ActionError`: when any action in a batch returns `ActionError`, skip all remaining actions in that batch and return `IterationOutcome::Continue` with the error in the summary. The LLM sees the error on the next iteration and can self-correct.

### Architecture

The change is in the action dispatch match block in each agent's `run_iteration` method. Currently:

```rust
// implementer.rs:400-414
match &result {
    ActionResult::Done(s) => return Ok(IterationOutcome::Done(s.clone())),
    ActionResult::NeedHelp(reason) => return Ok(IterationOutcome::NeedHelp(reason.clone())),
    ActionResult::BundleProposed(desc) => { /* ... return Done */ },
    _ => {}  // ActionError falls through here
}
summaries.push(summary);
```

After:

```rust
match &result {
    ActionResult::ActionError(_) => {
        summaries.push(summary);
        break;  // abort batch, feed error back via Continue
    }
    ActionResult::Done(s) => return Ok(IterationOutcome::Done(s.clone())),
    ActionResult::NeedHelp(reason) => return Ok(IterationOutcome::NeedHelp(reason.clone())),
    ActionResult::BundleProposed(desc) => { /* ... return Done */ },
    _ => {}
}
summaries.push(summary);
```

The `break` exits the `for action in &actions` loop, falling through to `Ok(IterationOutcome::Continue(summaries.join("\n")))`, which feeds the error back to the LLM on the next iteration.

### Files Changed

| File | Change |
|------|--------|
| `src/agents/implementer.rs` | Add `ActionError => break` arm in action dispatch match (line ~400) |
| `src/agents/coordinator.rs` | Add `ActionError => break` arm in action dispatch match (line ~1872) |
| `src/agents/researcher.rs` | Add `ActionError => break` arm in action dispatch match (line ~349) |

### Implementation Plan

**Single phase** - this is a small, focused change.

1. Add the `ActionError => { summaries.push(summary); break; }` match arm in all three agent action loops
2. Update existing tests that rely on actions after an `ActionError` being executed
3. Add a test: batch `[propose_bundle(noop), done]` where propose fails - assert `IterationOutcome::Continue` (not `Done`)
4. Run `otto ci`

## Alternatives Considered

### Alternative 1: Only suppress `done` after failed `propose_bundle`

- **Description:** Track a `suppress_done` flag specifically when `propose_bundle` returns `ActionError`, and skip `done` if the flag is set.
- **Pros:** Narrowly targeted, minimal behavior change
- **Cons:** Special-case logic; other action combinations could have the same problem (e.g., `[create_work, done]` where create fails). Adds a mutable flag variable to the loop.
- **Why not chosen:** The general rule ("errors abort the batch") is simpler and handles all combinations.

### Alternative 2: Make `ActionError` a hard iteration failure

- **Description:** Return `Err(...)` from the iteration instead of `Continue`, triggering the error path in the outer loop.
- **Pros:** Strongest guarantee that errors are never ignored
- **Cons:** Changes the contract - `ActionError` is currently documented as "non-fatal, fed back to the LLM." Making it fatal would change handback behavior and lifeguard counting. The outer loop treats `Err` as a session failure, which is too severe for "you forgot to commit."
- **Why not chosen:** `ActionError` is the right severity. The problem isn't that it's non-fatal - it's that a subsequent `done` prevents it from feeding back.

### Alternative 3: Pre-validate action batches before execution

- **Description:** Scan the batch for incompatible combinations (e.g., `propose_bundle` + `done`) and reject the batch before executing any action.
- **Pros:** Prevents the problem at the source
- **Cons:** Hard to enumerate all invalid combinations. The LLM might legitimately batch `[write_file, done]` or `[commit, propose_bundle, done]`. Overly restrictive.
- **Why not chosen:** Too brittle. The "break on error" approach is more general and doesn't require enumerating valid combinations.

## Technical Considerations

### Coordinator lifeguard interaction

The coordinator already has explicit `ActionError` handling (coordinator.rs:1810-1822) that feeds errors to the lifeguard before the dispatch match. The `break` would happen after that, so lifeguard tracking is unaffected.

### Implementer tool error correction

The implementer has a tool-error self-correction path (implementer.rs:330-366) that fires before the dispatch match. If correction succeeds, the result is no longer `ActionError`. If correction fails, the result stays `ActionError` and the `break` fires. This is correct - a failed correction should abort the batch.

### Testing Strategy

- Unit test: mock LLM returns `[propose_bundle(noop), done]`, worktree has uncommitted files. Assert `IterationOutcome::Continue` with error in summary.
- Unit test: mock LLM returns `[write_file, done]` where write succeeds. Assert `IterationOutcome::Done` (no error, no break).
- Unit test: mock LLM returns `[create_work(invalid), assign_agent]` where create fails. Assert `Continue` and that `assign_agent` was not called.
- Verify existing tests still pass.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Breaking valid batches where an ActionError is intentionally non-fatal | Low | Med | ActionError is already documented as "fed back to LLM" - aborting the batch is consistent with that intent |
| LLM gets stuck in error correction loop (error -> retry -> same error) | Med | Low | Lifeguard already detects repeated errors and escalates to NeedHelp |
| Behavioral change in coordinator/researcher action loops | Low | Low | Same pattern applied consistently; coordinator already has lifeguard handling for ActionError |

## Open Questions

None - all edge cases resolved during review.

## References

- v0.1.49: `determine_work_handback` fix (removed `if succeeded` short-circuit)
- v0.1.46: noop bundle dirty-worktree guard
- E2E failure logs: `~/.local/share/loopr/sessions/20260401T231520/agents/implementer-ag-ljuha.log`
