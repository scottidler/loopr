# Design Document: E2E Stability Fixes

**Author:** Scott A. Idler
**Date:** 2026-04-06
**Status:** Implemented
**Review Passes Completed:** 4/4

## Summary

Loopr's python-api e2e target has failed repeatedly (3x timeout, 1x killed) due to three intersecting root causes: implementers hallucinating noop bundles when files already exist, tools not being registered before use, and reviewer assignment being LLM-driven rather than automatic. This document specifies seven targeted fixes to address these failure modes permanently, organized into three phases by complexity.

## Problem Statement

### Background

The python-api e2e run on 2026-04-06 exhibited the same failure pattern as the previous three runs. One work item (`wk-kkbu1`) implemented the entire application (scope creep), causing every downstream work item to submit noop bundles claiming the files already existed. The reviewer correctly rejected every noop because it could not verify claims without file contents. The coordinator reset each work item back to Ready. The implementer re-ran and submitted the same noop. The loop ran until the timeout killed the process. A second failure mode: every implementer in a fresh worktree called `run_tool 'test'` immediately, got "Tool not found", and wasted iterations trying to self-recover. A third failure mode: the coordinator LLM made routing errors (passing work IDs where bundle IDs were required), costing more iterations.

### Problem

1. **Noop rejection loop**: an implementer that finds files already complete submits a noop bundle with no file contents; the reviewer rejects it because it cannot verify; the implementer retries with no memory of the rejection and produces the same noop forever.
2. **Tool bootstrap failure**: tools are not pre-registered in new worktrees; implementers call non-existent tools and waste their iteration budget trying to figure out why.
3. **LLM-driven routing**: the coordinator LLM assigns reviewers by ID, makes ID-type errors (work ID vs bundle ID), and is responsible for decisions the Rust FSM should make deterministically.

### Goals

- Break the noop rejection loop permanently
- Eliminate tool-not-found failures in fresh worktrees
- Remove reviewer assignment from the coordinator's LLM surface area
- Give implementers scope boundaries that prevent writing files outside their assigned work
- Improve log signal-to-noise ratio for monitoring

### Non-Goals

- Changing the decomposer template validation logic
- Modifying the coordinator FSM state machine states
- Changing how plans/specs/phases are generated
- Addressing coordinator goal handling or multi-plan orchestration

## Proposed Solution

### Overview

Seven fixes in three phases. Phase 1 is pure configuration (no code). Phase 2 fixes context injection and pre-flight checks. Phase 3 addresses architectural routing and decomposer reliability.

### Phase 1: Configuration Fixes (no code changes)

**Fix 1: Pre-populate tools in e2e scaffold**

The e2e scaffolding for each target should write default tools into `loopr.yml` based on the project ecosystem. For python targets: `pytest` for testing, no formatter needed since Docker validates. This eliminates the "Tool not found" bootstrap failure entirely.

In `bin/e2e-targets/python-api.md`, add a `tools:` section to the generated `loopr.yml` (written by the `bin/e2e` scaffold step):

```yaml
tools:
  test:
    command: "docker compose run --rm test"
    description: "Run the full pytest suite via Docker"
  fmt:
    command: "echo 'no local fmt; Docker validates'"
    description: "No-op: formatting validated inside Docker"
```

Similarly for other targets: rust uses `cargo test`, `cargo fmt`; node uses `npm test`; lua uses the project's test runner.

**Fix 2: Coordinator poll logging - downgrade to trace**

`dispatch(method=coordinator.get_state)` fires every 5 seconds and dominates the log. In `src/daemon/handlers/system.rs` or wherever handler dispatch is logged, the coordinator state poll should be `trace!` not `debug!`. The coordinator itself also logs idle spins at `debug!` - those should also be `trace!`.

**Fix 3: Richer context truncation warning**

The log currently emits: `WARN Hierarchy section exceeds token budget, truncating`

This should include: which agent (using the new work-ID-prefixed format from Fix 4), the budget, and how many tokens were dropped:
```
WARN [implementer:wk-ve34q:ag-fo6rr] Hierarchy section truncated: 8420 tokens > 6000 budget, dropped 2420 tokens
```

**Fix 4: Work ID in agent log prefix**

Currently: `[implementer:ag-fo6rr]`
Should be: `[implementer:wk-ve34q:ag-fo6rr]`

This removes the need to look up which session is executing which task during monitoring.

### Phase 2: Context and Pre-flight Fixes

**Fix 5: Inject rejection reason into implementer retry context**

The `Bundle.verification` field stores the reviewer's rejection reason. When a bundle is rejected and the work transitions back to Ready/InProgress for retry, the rejection reason is not currently fed back to the next implementer iteration. The implementer picks up the work with a blank slate and reproduces the same noop.

Fix: in `build_implementer_summary` (`src/agents/implementer.rs:151`), look up the most recently rejected bundle for the work item. If one exists, append a `### Previous Bundle Rejected` section with the bundle's `verification` field (the reviewer's rejection reason). This appended text flows into `with_previous_summary` and appears in the implementer's context on the next iteration.

This applies to all rejection reasons, not just noops. A reviewer who rejects for wrong HTTP status codes will have that reason surfaced to the implementer's next attempt.

**Important dependency on Fix 6**: Fix 5 alone is harmful for noop scenarios. If the worktree already matches main and the implementer receives "your bundle was rejected: provide file contents", the LLM has no valid code to write and may fabricate dummy changes to force a diff. Fix 6 (pre-flight AC check) must handle the noop case *before* Fix 5 ever fires. With Fix 6 in place, work items whose AC are already satisfied get marked Done at the pre-flight stage, and Fix 5 only activates for genuine semantic failures where the implementer submitted wrong or incomplete code.

**Fix 6: Pre-flight acceptance criteria check**

Before spawning an implementer for a Ready work item, check whether the acceptance criteria are already satisfied against the current repo state. This short-circuits the noop loop at the root: if AC pass, mark the work Done without running an implementer.

The pre-flight check is a haiku LLM call that reads the files listed in `resource_tags`, presents their contents alongside the work's `acceptance_criteria` list, and asks: "does the current code satisfy all of these assertions?". If yes, the work skips implementation entirely. If no (or if `resource_tags` is empty), the executor proceeds normally and spawns an implementer.

This is an LLM call, not a regex or structural check, because acceptance criteria are semantic assertions ("function `get_bookmark` returns 404 for unknown IDs") not grep-able patterns.

**FSM constraint**: the Work FSM does not allow `Ready -> Done`. The `Ready` state can only transition to `InProgress`, `Blocked`, or `Abandoned`. The pre-flight check must route through the existing FSM path:

```
Ready -> InProgress (via executor, as today)
      -> pre-flight AC check runs
      -> if AC pass: InProgress -> InReview -> Integrated -> Done
         (fast-path: create a synthetic bundle with the existing file contents,
          auto-review passes, integrator merges the no-diff bundle)
      -> if AC fail: proceed to implementer as normal
```

Alternatively, add `Done(Coordinator)` to the `#[transitions(...)]` macro on `Ready` in `src/domain/work.rs`. This is a one-line FSM amendment:

```rust
// Before:
#[transitions(InProgress(Coordinator), Blocked(Coordinator), Abandoned(Coordinator))]
Ready,

// After:
#[transitions(InProgress(Coordinator), Blocked(Coordinator), Abandoned(Coordinator), Done(Coordinator))]
Ready,
```

The direct `Ready -> Done` path is cleaner: it avoids synthetic bundles and unnecessary FSM hops. The coordinator role is the correct actor since only the coordinator should be able to declare work complete without implementation.

Location: `src/agents/executor.rs:34-135` - add a pre-flight gate before transitioning Work to InProgress. If AC pass, transition `Ready -> Done` (requires FSM amendment above). The gate calls a new function `preflight_ac_check(work, repo_path) -> bool` using the haiku model.

This is the structural fix for scope creep fallout: if `wk-kkbu1` writes everything, the pre-flight check on `wk-ve34q` passes immediately and marks it Done without spinning up an implementer.

### Phase 3: Architectural Fixes

**Fix 7: Auto-triage bundles on Proposed, removing LLM from the reviewer routing path**

The daemon already has an `auto_start_reviewer` hook (`src/daemon/handlers.rs:245-261`), but it triggers when a bundle transitions to `Triaged` - after the coordinator has already triaged it. This means the coordinator LLM must call `triage_bundle { bundle_id: "<bd-*>" }` before a reviewer spawns, and the coordinator's LLM confused the ID type (`wk-rq0mm` instead of `bd-lsmvb`), causing the reviewer to never be spawned for those bundles.

**FSM constraint**: the Bundle FSM requires `Proposed -> Triaged` before `Triaged -> Reviewed`. This is enforced by the `#[transitions(...)]` macro - `Proposed` cannot jump to `Reviewed`. The Triaged step must exist in the FSM path.

Fix: do NOT remove the Triaged state. Instead, auto-triage deterministically in Rust: when a bundle transitions to `Proposed`, the daemon immediately performs `Proposed -> Triaged` as a deterministic Rust handler (no LLM), which then triggers the existing `auto_start_reviewer` hook. The full path becomes:

```
Implementer submits bundle -> Proposed
  -> Rust daemon auto-triages -> Triaged (deterministic, no LLM call)
  -> auto_start_reviewer hook fires -> Reviewer spawned
  -> Reviewer completes -> Reviewed
  -> Coordinator accepts/rejects (this is where LLM reasoning belongs)
```

Implementation: in the `auto_start_agents` function (`src/daemon/handlers.rs:220-262`), add a new condition that intercepts `bundle.transition` to `Proposed` and immediately dispatches a second `bundle.transition` to `Triaged` with role `Coordinator`. This preserves the FSM path and keeps the existing `auto_start_reviewer` hook intact.

The coordinator's `triage_bundle` action should be removed from its action set since triage is now automatic. The coordinator only needs `accept_bundle` and `reject_bundle` on `Reviewed` bundles.

The `auto_start_reviewer: true` config flag in e2e targets already enables the Triaged -> Reviewer hook.

**Fix 8: Decomposer structured outputs**

The decomposer currently sends a plain text prompt and parses the response as a JSON array of child documents, stripping markdown fences manually. LLMs frequently produce invalid JSON escaping when the content contains Markdown, causing parse failures and retries.

Fix: use Claude's tool-use API to define a schema for the decomposer's expected output. The model fills in structured fields natively, eliminating manual JSON escaping. Alternatively, use delimiter-based output (`--- BEGIN spec.md ---`) parsed by Rust.

Location: `src/decomposer.rs:181-245` - replace `call_llm_for_children` with a tool-use call.

### Architecture

```
Current flow for noop loop:
  Work(Ready) -> Implementer(reads files, finds them complete)
                    -> Bundle(Proposed, noop_reason set, no touched_paths)
                    -> Coordinator(Triages) -> Reviewer(rejects: no file contents)
                    -> Work(Ready, no rejection context)
                    -> Implementer(fresh context, same files, same noop)
                    -> [loop forever]

Fixed flow (Fixes 5+6):
  Work(Ready) -> Pre-flight AC check
                    -> AC pass? -> Work(Done) [no implementer spawned]
                    -> AC fail? -> Implementer(context includes rejection reason if any)
                                    -> Bundle(with actual file contents)
                                    -> [normal review flow]

Current reviewer routing:
  Bundle(Proposed) -> Coordinator LLM calls triage_bundle(bd-*) -> Triaged
                   -> Rust daemon sees Triaged -> auto_start_reviewer -> Reviewer spawned
                   -> [LLM errors: passes wk-* instead of bd-*, reviewer never spawns]

Fixed reviewer routing (Fix 7):
  Bundle(Proposed) -> Rust daemon auto-triages (deterministic) -> Triaged
                   -> auto_start_reviewer hook fires -> Reviewer spawned -> Reviewed
                   -> Coordinator only sees Reviewed bundles, calls accept/reject
                   -> No ID routing by coordinator LLM; FSM path preserved
```

### Data Model

No new fields required. One FSM amendment required:

**Work FSM amendment (Fix 6):**
```rust
// src/domain/work.rs - add Done(Coordinator) to Ready transitions
#[transitions(InProgress(Coordinator), Blocked(Coordinator), Abandoned(Coordinator), Done(Coordinator))]
Ready,
```

This allows the pre-flight AC check to short-circuit `Ready -> Done` when acceptance criteria are already satisfied. The `Coordinator` role is the correct actor since only the coordinator should declare work complete without implementation.

**No Bundle FSM amendment needed (Fix 7):** the auto-triage approach preserves the existing `Proposed -> Triaged -> Reviewed` path. The change is behavioral (Rust auto-dispatches `Proposed -> Triaged`) not structural.

Existing fields used:
- `Bundle.verification` - read during context building (Fix 5)
- `Work.resource_tags` - pre-flight reads these files (Fix 6)
- `Work.acceptance_criteria` - pre-flight validates these (Fix 6)
- `Bundle.status` - auto-triage on Proposed triggers Triaged -> reviewer auto-spawn (Fix 7)

### Implementation Plan

**Phase 1 (configuration only):**
1. Add `tools:` section to each e2e target scaffold (Fix 1)
2. Downgrade coordinator poll logs to `trace!` (Fix 2)
3. Enrich truncation warning with agent ID, budget, dropped count (Fix 3)
4. Add work ID to agent log prefix format (Fix 4)

**Phase 2 (context enrichment + FSM amendment):**
1. Amend Work FSM: add `Done(Coordinator)` to `Ready` transitions in `src/domain/work.rs`
2. Add pre-flight AC check in `executor.rs` - if AC pass, transition `Ready -> Done` (Fix 6)
3. Implement rejection reason injection in `build_implementer_summary` (Fix 5) - depends on Fix 6 handling noops first

**Phase 3 (architectural routing):**
1. Add auto-triage handler: when bundle reaches `Proposed`, Rust daemon dispatches `Proposed -> Triaged` deterministically (Fix 7)
2. Existing `auto_start_reviewer` hook on `Triaged` fires as before - no FSM changes needed
3. Remove `triage_bundle` from coordinator action set; coordinator uses `accept_bundle` / `reject_bundle` on Reviewed bundles only
4. Decomposer structured outputs via tool-use API (Fix 8)

## Alternatives Considered

### Alternative: Noop auto-approve
- **Description:** when a reviewer receives a noop bundle, auto-read the files and verify AC directly rather than rejecting
- **Pros:** simpler than pre-flight; reviewer already has file-read tools
- **Cons:** still burns an agent session and LLM call; doesn't prevent the implementer from wasting its budget getting to the noop
- **Why not chosen:** pre-flight is strictly cheaper and catches the problem earlier

### Alternative: Scope hard-block in bundle handler
- **Description:** reject any bundle whose `touched_paths` contains files not in `resource_tags`
- **Pros:** prevents scope creep at submission time
- **Cons:** doesn't help when `resource_tags` is empty; the first implementer's bundle had no resource_tags set
- **Why not chosen:** complementary to Fix 6 but not sufficient alone; resource_tags population needs to be reliable first

### Alternative: Coordinator-driven phase advancement
- **Description:** coordinator explicitly marks all work in a phase Done when one item implements everything
- **Pros:** no pre-flight check needed
- **Cons:** requires LLM to reason about file contents across all work items; error-prone
- **Why not chosen:** pre-flight is deterministic; coordinator reasoning is not

### Alternative: Remove Proposed->Triaged step entirely
- **Description:** bundle FSM goes Proposed -> Reviewed (skip Triaged), coordinator only acts on Reviewed
- **Pros:** simplifies the FSM; coordinator never touches bundles until review is done
- **Cons:** loses the triage step where coordinator can reject obviously bad bundles before reviewer wastes a call
- **Why not chosen:** triage is valuable when it works; moving auto-spawn to Proposed preserves the triage step while removing ID-routing errors

## Technical Considerations

### Dependencies

Fix 6 (pre-flight AC check) requires the implementer's work context to include `resource_tags` reliably. If `resource_tags` is empty for a work item, the pre-flight check cannot validate and must fall back to spawning the implementer. The decomposer must populate `resource_tags` during work item creation.

Fix 7 (auto-spawn reviewer) requires ensuring only one reviewer session per bundle. The daemon must check for an existing non-terminal reviewer session before spawning.

Fix 8 (structured outputs) requires updating the decomposer's HTTP client to use the tools API. This is additive and does not change the decomposed document format.

### Performance

Pre-flight AC check (Fix 6) adds one file-read pass per work item before implementation. For 33 work items, this is trivial. For very large decompositions it adds latency but eliminates entire agent sessions, so net cost is negative.

Auto-spawn reviewer (Fix 7) removes one coordinator iteration per bundle (the `assign_agent` call), which is a net reduction in LLM calls.

### Testing Strategy

Fix 1 (tools scaffold): verified by running the e2e scaffold and checking `loopr.yml` contains the expected tools section; implementer should not see "Tool not found" in any run.

Fix 5 (rejection injection): unit test in context builder asserting that when a work item's most recent bundle is Rejected, the verification text appears in the built context.

Fix 6 (pre-flight): unit test in executor asserting that when a work item's AC are satisfied by current repo state, the work transitions to Done without an implementer session being created.

Fix 7 (auto-reviewer): unit test in `src/daemon/handlers.rs` asserting that a `bundle.transition` to Proposed triggers exactly one reviewer session start; a subsequent transition on the same bundle does not spawn a second reviewer (dedup guard).

### Rollout Plan

Run e2e in this order after each phase to validate before proceeding:
- After Phase 1: run `rust-version` (fast baseline) and `python-api` to confirm no tool-not-found errors
- After Phase 2: run `python-api` and `react-todo` to confirm no noop loops
- After Phase 3: run full suite (`lua-todo`, `python-api`, `react-todo`, `rust-cli`)

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Pre-flight AC check produces false positives (passes when code is wrong) | Med | Med | AC check uses same criteria as reviewer; if reviewer would approve, pre-flight should too; validate with rust-version first |
| Auto-spawn reviewer races (two reviewers spawned for one bundle) | Low | Med | Add dedup guard in bundle handler: check for existing non-terminal reviewer session before spawning |
| Decomposer tool-use API changes output format unexpectedly | Low | High | Pin to a specific tool schema; add regression test asserting structure |
| resource_tags empty for some work items; pre-flight can't validate | High | Low | Fallback to spawning implementer when resource_tags is empty; separately fix decomposer to always populate resource_tags |

## Open Questions

- [ ] Why does `work_queue::next_assignable_work` report no Ready work when 30 items are in Ready state? Most likely: those 30 items have dependencies on `wk-ve34q` or `wk-rq0mm` (InProgress/InReview), which the dependency filter (`unwrap_or(false)` for non-Done deps) correctly blocks. Needs confirmation by inspecting `dependencies` fields on the 30 blocked works in the JSONL. If correct, this is expected behavior, not a bug.
- [ ] Should `resource_tags` scope enforcement be a hard block (bundle rejected) or soft (warning in context)? Currently soft - should it become hard once resource_tags is reliably populated?
- [ ] Should the coordinator retain a `reject_bundle` action on `Triaged` bundles (pre-review fast-reject for obviously bad submissions), or should it only act on `Reviewed` bundles?

## References

- `src/daemon/work_queue.rs` - worker work-pickup logic
- `src/agents/context.rs` - implementer context assembly
- `src/agents/implementer.rs:244-254` - implementer summary building
- `src/daemon/handlers/bundle.rs` - bundle FSM transitions
- `src/agents/executor.rs:34-135` - implementer spawning
- `src/domain/bundle.rs:74` - verification field
- `src/domain/work.rs:52` - resource_tags field
- `src/decomposer.rs:181-245` - LLM child generation
- `bin/e2e-targets/python-api.md` - e2e scaffold target
- `docs/design/2026-03-05-chat-agentic-tool-loop.md` - tool registry design
