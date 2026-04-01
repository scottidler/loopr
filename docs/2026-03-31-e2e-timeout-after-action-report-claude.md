# After-Action Report: Python Todo E2E Run

**Date:** 2026-03-31
**Version:** v0.1.31
**Target:** python-todo (YAML manifest)
**Result:** Timeout (exit 1) after 900s

## Result

Only `todo.py` was created. `cli.py` and `test_todo.py` never started.
The run never reached GoalComplete.

## What Worked

- **YAML manifest injection:** Exactly 3 work items created with correct
  dependencies. The coordinator did not hallucinate extra tasks. This was
  the first run using deterministic plan injection, and it worked perfectly.
- **Dependency enforcement:** cli-entry and test-suite correctly blocked on
  todo-model. Workers respected the dependency graph.
- **Implementer:** Wrote valid `todo.py` with full TodoStore class (CRUD,
  JSON persistence, filtering) on the first try.
- **Reviewer acceptance gate:** First reviewer approved correctly, citing
  specific acceptance criteria. Noted that missing `test_todo.py` is a
  sibling work item, not a blocker. The new acceptance-criteria-first
  reviewer prompt worked as designed.
- **Lifeguard circuit breaker:** Fired correctly when the coordinator looped
  on invalid transitions (3 repeated errors). Prevented infinite loops.
- **Coordinator FSM:** Correctly entered Executing state, monitored work
  items, used override_work to reset stuck work items.

## Two Bugs Found

### Bug 1: Validation command deadlock (E2E test config)

The integrator runs `.venv/bin/python -m pytest test_todo.py -v` after every
bundle merge. But `test_todo.py` is Work 3 - it doesn't exist until
todo-model (Work 1) reaches Done. Work 1 can never reach Done because its
merged tick always fails validation. Deadlock.

The integrator was doing exactly what it was told. This was not a machinery
bug - it was a test configuration that assumed a finished project while the
execution model builds incrementally.

**Fix:** Conditional validation that's aware of partial states:
```
test -f test_todo.py && .venv/bin/python -m pytest test_todo.py -v || test ! -f test_todo.py
```

### Bug 2: System prompt truncation (machinery bug)

The reviewer's `.pmt` template is 2245 chars (~562 tokens). The
`TokenBudget.system_prompt` field was 500 tokens. `truncate_prose()` cuts
from the tail, which is where the Output Format JSON schema lives.

Result: the first reviewer got lucky and produced correct JSON. All 7
subsequent reviewers produced wrong field names (`findings` instead of
`issues`, `rejected` instead of `reject`) because they never saw the schema.
Parse failure, agent dies.

This was the more consequential bug. The system was silently lobotomizing
its own instructions.

**Fix:** Removed `system_prompt` from `TokenBudget` entirely. The `.pmt`
templates are our source code - they are never truncated. `build()` now
returns `Result<AssembledContext>` and hard-errors if the total assembled
context exceeds the model's input limit (190k tokens). The system either
executes with perfect instructions or fails loudly.

## Timeline

```
17:09  Manifest seeded, coordinator executing (3 works, 1 phase)
17:09  Worker picks up wk-1ie7g, implementer writes todo.py
17:12  Reviewer ag-a08op approves bundle bd-0p2ls (acceptance gate works)
17:12  Integrator merges tick, runs pytest test_todo.py -> FAIL (not found)
17:12  Bundle rejected, work reset to Ready
17:13  Cycle repeats: implement -> review (7 parse failures) -> stuck
17:18  Coordinator hits Lifeguard, transitions to Failed
17:28  Daemon restarts, same cycle continues
17:24  Timeout reached, exit 1
```

## Lessons

### Never silently degrade your own instructions

The system prompt is the contract between us and the LLM. We wrote it, we
control it, it's not variable data. Treating it as something that might need
truncation was a category error baked into the original design. When it got
truncated, the system didn't crash - it kept running with an LLM that didn't
know what format to respond in. That's worse than a crash.

### Budget systems need categories, not just numbers

The original design had one concept: "token budget per section." But there
are two fundamentally different kinds of content - static instructions we
control and variable data we don't. Applying the same truncation logic to
both was the root architectural mistake. The fix wasn't bumping numbers - it
was recognizing the categories are different and treating them differently.

### Validation must match the execution model

The validation command assumed a finished project, but the execution model
builds incrementally. Validation that references artifacts from future work
items creates a deadlock. This isn't unique to E2E tests - any real project
with phased delivery will hit this if validation is only global.

### The first success can mask a systemic failure

The first reviewer got lucky. One success out of eight is a 12.5% success
rate. If we'd only looked at the first review, we'd have missed the bug
entirely.

## Changes Made

| File | Change |
|------|--------|
| `src/agents/context.rs` | Removed `system_prompt` from `TokenBudget`. `build()` returns `Result`. Hard error on context overflow. |
| `src/agents/coordinator.rs` | Added `?` to `build()` call |
| `src/agents/implementer.rs` | Added `?` to `build()` calls |
| `src/agents/researcher.rs` | Added `?` to `build()` call |
| `src/agents/reviewer.rs` | Added `?` to `build()` call |
| `bin/e2e-targets/python-todo.sh` | Conditional validation command |
