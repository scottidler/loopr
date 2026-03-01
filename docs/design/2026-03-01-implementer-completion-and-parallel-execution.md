# Design Document: Implementer Completion & Parallel Execution

**Author:** Scott Idler
**Date:** 2026-03-01
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

The second live end-to-end run of loopr v3 exposed two systemic failures that prevent the system from functioning as a parallel autonomous build orchestrator: (1) implementer agents waste their entire iteration budget in read/build loops without ever completing, and (2) the coordinator creates serial dependency chains between works that should run in parallel, reducing the system to a single-threaded pipeline.

## Problem Statement

### Background

After the first live-run fixes (validator bypass, tool detection, coordinator supervisor, lifeguard tuning), the system was re-tested with a goal of "Build a Rust CLI todo app." The coordinator successfully generates Plans, Specs, Phases, and Works. Implementers spawn, write correct code, and pass builds/tests. The reviewer and integrator pipeline works. But the system cannot complete a goal because implementers burn 20 iterations without finishing, and only one implementer runs at a time.

### Problem

Two compounding bugs make the system non-viable:

1. **Implementer read loop** — After successfully writing files and passing `cargo build` (iteration 2), the implementer enters an infinite cycle of `read_file` → `run_tool build` → `read_file` → `run_tool build`, never progressing to `commit` → `propose_bundle` → `done`. It hits the 20-iteration cap every time, force-proposes a WIP bundle, and exits as `Failed`. The work gets through only because the coordinator triages the force-proposed bundle and the reviewer approves it — but this wastes 18 iterations per work item and marks every implementer as failed.

2. **Serial work dependencies** — The coordinator creates works with linear dependency chains (`Work1 → Work2 → Work3 → Work4`) when the works touch independent files and could run in parallel. For a 4-work phase, this means 4 sequential implementer sessions instead of 2-3 parallel ones. Combined with bug #1 burning 20 iterations per work, a simple phase takes 80+ iterations sequentially instead of ~10 iterations across 3 parallel agents.

### Root Cause Analysis

#### Bug 1: Implementer Read Loop

The implementer's `previous_summary` field (the only memory it has between iterations) was **replaced** each iteration, not accumulated. After iteration 2 where it wrote files and ran build, iteration 3's context contained:

```
## Previous Iteration Summary
ran build (exit 0)
stderr: Finished `dev` profile...
```

The LLM had **zero knowledge** that it wrote files in iteration 1. It saw a successful build but didn't know what files existed, so it read them "to check." Iteration 4 then saw "read Cargo.toml (244 bytes)" and ran build again. The cycle repeated indefinitely.

Additionally, the implementer prompt instructed "Work in this order: (1) read, (2) write, (3) test, (4) clippy, (5) fmt, (6) commit, (7) propose" but provided no mechanism for the LLM to track which steps were complete. Combined with the single-iteration summary, the LLM lost its place every iteration.

Four secondary deserialization failures compounded the problem:
- `"args": {}` on `run_tool` actions failed `string_or_vec` (no `visit_map` handler)
- `"files"` on `commit` actions was ignored (struct expects `"paths"`)
- `"summary"` on `propose_bundle` actions was ignored (struct expects `"description"`)
- `Done` without `summary` field failed deserialization (field was required)

Each parse failure consumed an iteration and reset the LLM's context to an error message, further destroying state continuity.

#### Bug 2: Serial Work Dependencies

The `generation-work.pmt` prompt instructs:

```
Declare dependencies on other Works:
  - For existing Works: use their exact IDs
  - For Works in this same batch: use "batch:0" (first item), "batch:1" (second), etc.

Order Works so dependencies come first. Items with no dependencies should be listed first.
```

The instruction "Order Works so dependencies come first" nudges the LLM to think linearly. The `batch:N` syntax makes chaining trivial — the LLM naturally writes `batch:0` on item 1, `batch:1` on item 2, creating an A→B→C chain. The prompt contains no guidance about parallelism, resource-tag independence, or fan-out patterns.

The `coordinator.pmt` says "MAXIMIZE PARALLELISM" (added during this session) but this instruction appears in the system prompt, while the `generation-work.pmt` instructions appear in the user message footer — closer to the LLM's response and higher salience. The conflicting signals result in the user-message instructions winning.

### Goals

- Implementers complete works in 3-5 iterations consistently, finishing as `Completed` not `Failed`
- Multiple implementers run in parallel on independent works within a phase
- The coordinator creates fan-out dependency graphs by default, only chaining works that share resource_tags
- Zero parse failures from common LLM output variations (`args: {}`, `files: [...]`, `summary: "..."`, bare `done`)

### Non-Goals

- Changing the 4-level hierarchy (Plan → Spec → Phase → Work)
- Multi-phase parallelism (phases remain sequential)
- Implementer tool selection intelligence (covered by project-aware tool detection)
- Reviewer or integrator changes

## Proposed Solution

### Overview

Seven changes across three layers: serde robustness (deserialization), implementer state management (agent loop + context truncation), and work decomposition (coordinator prompts + optional code enforcement).

### Fix 1: Accumulated Iteration History

**File:** `src/agents/implementer.rs` (line 371)

**Before:** `self.previous_summary = Some(summary);` — replaces each iteration.

**After:** Accumulate with iteration markers:
```rust
let entry = format!("--- Iteration {} ---\n{}", i, summary);
self.previous_summary = Some(match self.previous_summary.take() {
    Some(prev) => format!("{}\n{}", prev, entry),
    None => entry,
});
```

The implementer's token budget for `previous_summary` is 4000 tokens (`context.rs:160`). The `build()` method already truncates via `truncate_prose()` when exceeded (`context.rs:597-599`).

**Bug:** `truncate_prose()` keeps `text[..max_chars]` — it preserves the **oldest** iterations and drops the **newest**. This is backwards. The newest iterations contain the most relevant state (what just happened), while the oldest (initial file reads) are least useful. This truncation direction must be reversed as part of this fix — keep the tail, drop the head.

**Status:** Implemented and tested this session.

### Fix 2: Auto-Done on Bundle Proposal

**File:** `src/agents/implementer.rs` (lines 252-266)

When `execute_action` returns `ActionResult::BundleProposed`, the iteration loop now returns `IterationOutcome::Done` instead of `IterationOutcome::Continue`. Proposing a bundle is the terminal action — there's no valid reason to continue iterating after a successful proposal.

```rust
ActionResult::BundleProposed(desc) => {
    summaries.push(summary);
    let done_summary = if desc.is_empty() {
        summaries.join("\n")
    } else {
        format!("{}\n{}", summaries.join("\n"), desc)
    };
    return Ok(IterationOutcome::Done(done_summary));
}
```

This eliminates the failure mode where the LLM proposes a bundle but doesn't emit `done`, then continues reading files for the remaining iterations.

**Status:** Implemented and tested this session.

### Fix 3: Serde Robustness for LLM Output Variations

**File:** `src/agents/mod.rs`

Four changes to `AgentAction` enum:

| Field | Problem | Fix |
|-------|---------|-----|
| `RunTool.args` | `"args": {}` fails `string_or_vec` | Add `visit_map` handler returning `Ok(Vec::new())` |
| `Commit.paths` | LLM sends `"files"` | Add `#[serde(alias = "files")]` |
| `ProposeBundle.description` | LLM sends `"summary"` | Add `#[serde(alias = "summary")]` |
| `Done.summary` | LLM omits field entirely | Add `#[serde(default)]` |

**Status:** Implemented and tested this session. New test: `test_string_or_vec_empty_object`.

### Fix 4: Parallel-First Work Decomposition Prompt

**File:** `prompts/generation-work.pmt`

Replace the current dependency instructions with explicit parallelism-first guidance:

```
Declare dependencies between Works:
  - For existing Works: use their exact IDs
  - For Works in this same batch: use "batch:0" (first item), "batch:1" (second), etc.
  - ONLY add a dependency when a Work literally cannot compile or test without
    the other Work's output (e.g., Work B imports a struct defined in Work A's files).
  - If two Works touch DIFFERENT files, they MUST have NO dependency between them.
  - Prefer fan-out: many Works depending on one scaffolding Work, NOT linear chains.
  - Example — WRONG: A→B→C  RIGHT: A→B, A→C (B and C run in parallel after A)

List items with no dependencies first, then items with dependencies.
```

Also add to `coordinator.pmt` Rules section:
```
- When creating Works: if two Works have non-overlapping resource_tags, they MUST NOT
  depend on each other. Only add deps when files overlap or one Work's output is
  imported by another.
```

**Status:** Implemented. Both `coordinator.pmt` and `generation-work.pmt` updated.

### Fix 5: Implementer Workflow Prompt

**File:** `prompts/implementer.pmt`

Restructured to emphasize:
- Strict 5-step sequence: Read → Write → Verify → Fix → Ship
- "CRITICAL" callout: once tools pass, immediately commit+propose+done
- Iteration budget awareness: "re-reading files after successful tool runs is a failure mode"
- Example showing the complete 3-iteration flow
- Explicit instruction: `"args": []` not `"args": {}`

**Status:** Implemented this session.

### Fix 6 (Optional): Code-Level Dependency Pruning

**File:** `src/agents/coordinator.rs` (new function)

After the coordinator creates works in a batch, automatically prune dependencies between works whose `resource_tags` don't overlap. Since works are created one-at-a-time via IPC (each `CreateWork` action results in a `bridge.request("work.create", ...)` call), the pruning must happen **after the entire batch is created**, by reading back from the store:

```rust
fn prune_independent_deps(stores: &Stores, phase_id: &str, agent_log: &AgentLogger) {
    let mut works = stores.works.write().unwrap();
    let phase_work_ids: Vec<String> = works.values()
        .filter(|w| w.phase_id == phase_id)
        .map(|w| w.id.clone())
        .collect();

    // Build resource_tags lookup
    let tag_map: HashMap<String, HashSet<&str>> = works.iter()
        .filter(|(_, w)| w.phase_id == phase_id)
        .map(|(id, w)| (id.clone(), w.resource_tags.iter().map(|s| s.as_str()).collect()))
        .collect();

    for wi_id in &phase_work_ids {
        if let Some(wi) = works.get_mut(wi_id) {
            let my_tags = tag_map.get(wi_id).cloned().unwrap_or_default();
            let before = wi.dependencies.len();
            wi.dependencies.retain(|dep_id| {
                if let Some(dep_tags) = tag_map.get(dep_id) {
                    !my_tags.is_disjoint(dep_tags) // Keep only if resource_tags overlap
                } else {
                    true // Keep deps on works outside this phase
                }
            });
            if wi.dependencies.len() < before {
                agent_log.info(&format!(
                    "pruned {} independent deps from '{}'",
                    before - wi.dependencies.len(), wi.title
                ));
            }
        }
    }
}
```

This is a safety net — the prompt should get it right, but if the LLM still chains independent works, the code removes the unnecessary deps. Called from the coordinator's action execution loop after all `CreateWork` actions in a batch are processed.

**Status:** Implemented. Prunes deps between batch works with disjoint resource_tags.

### Fix 7: Reverse Truncation Direction for Previous Summary

**File:** `src/agents/context.rs` (lines 592-604)

`truncate_prose()` keeps the first N characters and drops the rest. For accumulated iteration history, this preserves the oldest iterations (initial file reads) and drops the newest (recent tool results, commits). This is backwards — the LLM needs the most recent context to know where it left off.

Add a `truncate_from_head()` function that keeps the **tail** of the text:

```rust
fn truncate_from_head(text: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens * 4;
    if text.len() <= max_chars {
        return text.to_string();
    }
    let start = text.len() - max_chars;
    // Find a clean break point (newline or sentence boundary)
    if let Some(pos) = text[start..].find('\n') {
        format!("[earlier iterations truncated]\n{}", &text[start + pos + 1..])
    } else {
        format!("[truncated] {}", &text[start..])
    }
}
```

Use this in `build()` specifically for the previous_summary section instead of `truncate_prose()`.

**Status:** Implemented. `truncate_from_head()` keeps tail, drops head.

### Implementation Status Summary

| Fix | Description | Status |
|-----|-------------|--------|
| 1 | Accumulated iteration history | Done, committed |
| 2 | Auto-done on bundle proposal | Done, committed |
| 3 | Serde robustness (4 changes) | Done, committed |
| 4 | Parallel-first work prompt | Done, committed (`coordinator.pmt` + `generation-work.pmt`) |
| 5 | Implementer workflow prompt | Done, committed |
| 6 | Code-level dependency pruning | Done, committed |
| 7 | Reverse truncation direction | Done, committed |

All 7 fixes implemented. Fixes 1-3 and 5 reduced implementer iterations from 20 (always hitting cap, `Failed`) to 2-7 (`Completed`). Parallelism has not been validated in a live run.

## Alternatives Considered

### Alternative 1: Full Conversation History

Instead of accumulated summaries, maintain the full LLM conversation (multi-turn) across iterations.

- **Pros:** LLM has perfect recall of all prior actions and results.
- **Cons:** Token cost grows linearly — a 20-iteration session would consume ~40K tokens of history. The implementer's budget is ~13K total. Would require a fundamentally different context architecture (conversation API instead of single-shot).
- **Why not chosen:** Too expensive, too architectural. Accumulated summaries with truncation achieve 80% of the benefit at 10% of the cost.

### Alternative 2: Structured State Tracker

Maintain a checklist in the implementer context showing completed steps:

```
Workflow State:
  [x] Read files (iteration 1)
  [x] Write files (iteration 2)
  [x] Run tests — PASSED (iteration 3)
  [ ] Run clippy
  [ ] Run fmt
  [ ] Commit
  [ ] Propose bundle
```

- **Pros:** Unambiguous — LLM always knows exactly where it is.
- **Cons:** Requires parsing LLM actions back into step tracking. Adds complexity to the implementer agent. Steps don't always follow this order (e.g., write→test→fix→rewrite→test).
- **Why not chosen for now:** The accumulated summary + auto-done on propose provides most of the benefit. Consider this if the read-loop persists after fixes 1+2.

### Alternative 3: Hard-Block Redundant Reads

In `execute_action`, track which files have been read and reject duplicate reads:

- **Pros:** Mechanically impossible to loop on reads.
- **Cons:** Legitimate re-reads after writes become impossible. The agent needs to re-read a file after modifying it to verify content.
- **Why not chosen:** Too restrictive. The LLM should be allowed to re-read after writing.

## Technical Considerations

### Dependencies

No new external dependencies. All changes are in existing modules.

### Performance

- Accumulated summaries grow the implementer's context by ~200 tokens per iteration. At 4000-token budget, this allows ~20 iterations of history before truncation. Net positive: fewer wasted iterations means less API spend.
- Parallel implementers increase concurrent LLM API calls. The `max_pool` config (default: 6) already gates this.

### Testing Strategy

Existing tests cover:
- `test_string_or_vec_empty_object` — new, validates `{}` → empty vec
- `test_run_implementer_completes_on_done` — existing, validates Done outcome
- `test_run_iteration_done_stops_remaining_actions` — existing, validates Done stops loop
- `test_has_proposed_true_skips_force_propose` — existing, validates force-propose skip

All 1679 tests pass after changes. No new test failures.

Additional testing needed:
- Live run with parallel implementers (2+ running simultaneously)
- Verify accumulated summary truncation doesn't lose critical context
- Verify auto-done on propose doesn't interfere with force-propose at iteration cap

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Accumulated summary exceeds token budget, truncates critical early iterations | Medium | Low | Budget is 4000 tokens (~20 iterations). Implementers should complete in 3-5. If they hit 20, they have bigger problems. |
| Auto-done on propose skips legitimate post-proposal actions | Low | Low | There are no legitimate post-proposal actions. Proposing is terminal by design. |
| Prompt changes still don't prevent LLM from chaining deps | High | High | Code-level dep pruning (Fix 6) as fallback. Monitor live runs. |
| Parallel implementers create merge conflicts in integrator | Medium | Medium | Worktree isolation already prevents filesystem conflicts. Git merge conflicts in the integrator are handled by existing retry logic. |
| Dependency pruning removes deps that are logically needed but have imprecise resource_tags | Low | High | resource_tags overlap check is conservative — if both works list `src/main.rs` (e.g., both add `mod` declarations), the dep is preserved. Only truly disjoint file sets get pruned. |
| Coordinator auto-start race: daemon boots with no goal, coordinator starts and idles, goal is set later but coordinator never re-evaluates | High | High | Every fresh daemon start hits this. Current workaround: kill and restart daemon after setting goal. Fix: either delay coordinator auto-start until a goal exists, or have the coordinator poll for goal changes during idle. Tracked separately but should be fixed before next live run. |

## Open Questions

- [ ] Should Fix 6 (code-level dep pruning) be implemented now or deferred until prompt changes are evaluated?
- [ ] Should the implementer track a structured workflow checklist (Alternative 2) in addition to accumulated summaries?
- [ ] Should the auto-start race condition (coordinator starts with no goal, never re-triggers) be fixed in this pass or tracked separately? This blocked every test run — required killing and restarting the daemon after setting the goal.
- [x] The `previous_summary` truncation uses `truncate_prose` which keeps the beginning and drops the end — this MUST be reversed for accumulated summaries (keep newest, drop oldest). Identified in pass 2.

## References

- `docs/design/2026-03-01-live-run-fixes.md` — First live-run bug fixes (validator, tools, supervisor, lifeguard)
- `docs/design/2026-02-26-loopr-v3-mvp4.md` — MVP4 design (coordinator, multi-level RWL)
- `docs/design/2026-02-26-loopr-v3-mvp3.md` — MVP3 design (implementer, reviewer agents)
- `prompts/implementer.pmt` — Implementer system prompt
- `prompts/coordinator.pmt` — Coordinator system prompt
- `prompts/generation-work.pmt` — Work generation instructions
