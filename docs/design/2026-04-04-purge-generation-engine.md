# Design Document: Purge the Legacy Generation Engine

**Author:** Scott A. Idler
**Date:** 2026-04-04
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

The Doc architecture (introduced in v3) moved Plan/Spec/Phase/Work decomposition
from the Coordinator's LLM loop into a dedicated Decomposer that runs before the
Coordinator starts. The legacy generation engine - the code that let the Coordinator
generate hierarchy documents itself - was not deleted. It was suppressed with
`#[allow(dead_code)]` and left in place. This document describes what is dead,
what is live, and how to surgically remove the dead code without disrupting the
active FSM paths.

---

## Problem Statement

### Background

Before the Doc architecture, the Coordinator ran an iterative LLM loop that:

1. Called `determine_generation_level()` to find the first missing hierarchy level
2. Built a prompt with `build_plan_prompt()` / `build_spec_prompt()` / etc.
3. Asked the LLM to emit `create_plan` / `create_spec` / `create_phase` / `create_work` JSON
4. Executed those actions via `handle_create_plan` / `handle_create_spec` / etc.

The Doc architecture replaced this with a synchronous Decomposer that runs
completely before the Coordinator starts:
`doc.accept` or `doc.inject` → `accept_plan_markdown()` → `decompose_hierarchy()` →
all Docs (and their double-written Plan/Spec/Phase/Work records) exist before the
Coordinator's first iteration.

### Problem

The legacy generation engine was never deleted. The entry point
(`build_generation_footer`) is `#[allow(dead_code)]` and has a deprecation comment,
but the functions it calls are still present, many of them exported, and the
`AgentAction::CreatePlan / CreateSpec / CreatePhase` variants are still listed in
`prompts/coordinator.pmt` as valid LLM actions.

This creates four concrete harms:

1. **Prompt hallucination risk.** The coordinator prompt tells the LLM it can emit
   `create_plan`, `create_spec`, `create_phase`. The LLM may use these in
   states where it is confused, producing actions that do nothing useful (handlers
   exist) or create phantom hierarchy records alongside the Doc-based records.

2. **Dead test mass.** `generation/tests.rs` (1415 lines, 75 tests) and the nine
   `test_build_generation_footer_*` tests in `coordinator/tests.rs` validate logic
   that is never executed in production. They burn CI time and mislead about what
   the system actually does.

3. **False API surface.** `generation.rs` exports 20+ functions. 15 are dead. New
   contributors see them and reason about a pipeline that does not run.

4. **Maintenance drag.** Every refactor that touches `Plan`, `Spec`, `Phase`, or
   `Work` must also not break the dead generation code or its tests.

### Goals

- Delete all code paths that were exclusively part of the pre-Doc generation engine
- Remove `create_plan`, `create_spec`, `create_phase` from the coordinator prompt
- Delete the corresponding `AgentAction` variants and executor handlers
- Preserve every code path that the Doc-based Coordinator actively uses
- Leave `otto ci` green after each phase

### Non-Goals

- Changing the Decomposer
- Changing `handle_create_work` or `AgentAction::CreateWork` - the `ActivatePhase`
  FSM state still asks the LLM to generate Works for each phase
- Changing `resolve_batch_dependencies` or `prune_independent_deps` - both are
  called from `coordinator/run.rs` for the CreateWork batch flow
- Removing `double_write_old_records` from `doc.rs` - that is a separate migration
- Changing the evaluator, validator, or coverage check paths

---

## Proposed Solution

### Overview

Delete in six independent phases, each verified with `cargo check` or `otto ci`.
Each phase is a clean compile checkpoint.

### What Is Dead vs Live

**In `src/agents/coordinator.rs`:**

| Function | Status | Reason |
|----------|--------|--------|
| `build_generation_footer` | DEAD - `#[allow(dead_code)]`, never called | Entry point for old generation loop |
| `query_learnings_for_level` | DEAD - `#[allow(dead_code)]`, only called from `build_generation_footer` | |
| `find_pending_draft_for_validation` | DEAD - `#[allow(dead_code)]`, only called from `build_generation_footer` | |
| `resolve_batch_dependencies` | LIVE - called from `coordinator/run.rs:256` | CreateWork batch dependency resolution |
| `prune_independent_deps` | LIVE - called from `coordinator/run.rs:548` | CreateWork dep pruning |
| `build_state_summary_with_sla` | LIVE - called by coordinator loop | |

**In `src/agents/generation.rs`:**

| Function | Status | Reason |
|----------|--------|--------|
| `build_plan_prompt` | DEAD | Only called inside `build_generation_footer` |
| `build_spec_prompt` | DEAD | Only called inside `build_generation_footer` |
| `build_phase_prompt` | DEAD | Only called inside `build_generation_footer` |
| `build_work_prompt` | LIVE | Called at coordinator.rs:1135 in `ActivatePhase` state |
| `determine_generation_level` | DEAD | Only called inside `build_generation_footer` |
| `is_validation_cap_reached` | DEAD | Only called inside `build_generation_footer` |
| `find_draft_needing_regeneration` | DEAD | Only called inside `build_generation_footer` |
| `find_incomplete_decomposition` | DEAD | Only called inside `build_generation_footer` |
| `is_decomposition_cap_reached` | DEAD | Only called inside `build_generation_footer` |
| `get_coverage_gaps` | DEAD | Only called inside `build_generation_footer` |
| `find_pending_coverage_check` | DEAD | Only called inside `build_generation_footer` |
| `find_phase_needing_works` | DEAD | Only called inside `build_generation_footer` |
| `gather_parent_context` | DEAD | Only called from tests |
| `collect_failure_messages` | DEAD | Only called from tests |
| `find_failed_validations` | DEAD | Only called from tests |
| `find_latest_coverage_report` | DEAD | Zero callers anywhere |
| `find_active_plan` | LIVE | Called from `build_state_summary_with_sla`, FSM loop |
| `find_active_specs_for_plan` | LIVE | Called from FSM loop |
| `find_active_phases_for_spec` | LIVE | Called from FSM loop |
| `is_phase_complete` | LIVE | Called from FSM loop |
| `find_works_for_parent` | LIVE | Called from `ActivatePhase` and FSM loop |

**In `src/agents/action.rs`:**

| Variant | Status | Reason |
|---------|--------|--------|
| `AgentAction::CreatePlan` | DEAD | No live path emits it; `build_generation_footer` dead |
| `AgentAction::CreateSpec` | DEAD | Same |
| `AgentAction::CreatePhase` | DEAD | Same |
| `AgentAction::CreateWork` | LIVE | `ActivatePhase` FSM state asks LLM to emit this |
| `AgentAction::ProposePlan` | LIVE | `Interviewing` FSM state |

**In `src/agents/executor/action/record.rs`:**

| Handler | Status |
|---------|--------|
| `handle_create_plan` | DEAD - variant deleted |
| `handle_create_spec` | DEAD - variant deleted |
| `handle_create_phase` | DEAD - variant deleted |

**In `src/agents/executor/action/work.rs`:**

| Handler | Status |
|---------|--------|
| `handle_create_work` | LIVE - `CreateWork` variant is live |

**Dead prompt files:**

| File | Status | Used by |
|------|--------|---------|
| `prompts/generation-plan.pmt` | DEAD | `build_plan_prompt` only |
| `prompts/generation-spec.pmt` | DEAD | `build_spec_prompt` only |
| `prompts/generation-phase.pmt` | DEAD | `build_phase_prompt` only |
| `prompts/generation-work.pmt` | LIVE | `build_work_prompt` (called from `ActivatePhase`) |

The `PromptStore` struct in `src/prompts.rs` has fields `generation_plan`, `generation_spec`,
`generation_phase` that load the dead files. These must be removed with the files.
`generation_work` must be kept.

**Existing tests that assert the wrong thing:**

Three tests currently assert the dead actions ARE in the prompt. They will break when the
prompt is fixed, but they are written backwards - they validate the bug, not the correct
behavior. They must be fixed in Phase 1 alongside the prompt change:

| File | Test | Current (wrong) | Fix |
|------|------|-----------------|-----|
| `src/prompts.rs` | `test_coordinator_pmt_identity` | checks `create_plan`, `create_spec`, `create_phase` are in action list | remove those three from the checked list; add `!contains` assertions for each |
| `src/agents/coordinator/tests.rs` | `test_system_prompt_contains_key_sections` | `assert!(prompt.contains("create_plan"))` | delete that line; add `assert!(!prompt.contains("create_plan"))` |
| `src/prompts.rs` | `test_generation_plan_pmt_content` | asserts `create_plan` in generation-plan prompt | delete (prompt file deleted in Phase 6) |
| `src/prompts.rs` | `test_generation_spec_pmt_content` | asserts `create_spec` in generation-spec prompt | delete (prompt file deleted in Phase 6) |
| `src/prompts.rs` | `test_generation_phase_pmt_content` | asserts `create_phase` in generation-phase prompt | delete (prompt file deleted in Phase 6) |

**Dead tests:**

- `src/agents/generation/tests.rs` - all 75 tests: every test covers a dead function
  (`build_plan_prompt`, `build_spec_prompt`, `build_phase_prompt`, `determine_generation_level`,
  `find_draft_needing_regeneration`, `is_validation_cap_reached`, `gather_parent_context`,
  `collect_failure_messages`, `find_failed_validations`, `find_pending_coverage_check`,
  `find_incomplete_decomposition`, `is_decomposition_cap_reached`). Even the tests
  for live functions (`find_active_plan`, `find_works_for_parent`, `is_phase_complete`)
  belong in `generation.rs` unit tests, not in a separate test file for a dead module.
- Nine `test_build_generation_footer_*` tests in `coordinator/tests.rs`
- `test_execute_create_plan`, `test_execute_create_spec`, `test_execute_create_phase`
  (and their error_path variants) in `executor/action/record.rs`
- `CreatePlan` / `CreateSpec` / `CreatePhase` serde tests in `action.rs`
- `src/tests/integration/executor.rs` - tests for `CreatePlan` → `CreateSpec` →
  `CreatePhase` → `CreateWork` pipeline that no longer exists
- `src/tests/integration/errors.rs` - `test_create_spec_with_invalid_plan_id_returns_error`

### Implementation Plan

#### Phase 1 - Remove `create_plan`, `create_spec`, `create_phase` from coordinator prompt and fix backwards tests

Edit `prompts/coordinator.pmt` to remove the three dead action definitions.
Keep `create_work` and `propose_plan`.

Fix the two tests that currently assert the wrong thing (validating the bug):

- `src/prompts.rs::test_coordinator_pmt_identity`: remove `create_plan`, `create_spec`,
  `create_phase` from the action list that is asserted to be present; add explicit
  `assert!(!p.contains("create_plan"))` etc. so the test actively guards against regression.
- `src/agents/coordinator/tests.rs::test_system_prompt_contains_key_sections`: remove
  `assert!(prompt.contains("create_plan"))`; add `assert!(!prompt.contains("create_plan"))`.

Note: `test_generation_plan_pmt_content`, `test_generation_spec_pmt_content`,
`test_generation_phase_pmt_content` are left for Phase 6 when the prompt files they test
are deleted.

Verify: `otto ci` passes. No LLM sees these actions. Tests now guard against re-introduction.

#### Phase 2 - Delete `AgentAction::CreatePlan`, `CreateSpec`, `CreatePhase` variants

From `src/agents/action.rs`, remove the three enum variants and their serde parsing
tests. Follow compiler errors to remove the dispatch arms in
`src/agents/executor/action.rs` (lines 100-114).

Verify: `cargo check` passes.

#### Phase 3 - Delete executor handlers for the deleted variants

Delete from `src/agents/executor/action/record.rs`:
- `handle_create_plan` (and its tests: `test_execute_create_plan`,
  `test_execute_create_plan_error_path`)
- `handle_create_spec` (and its tests: `test_execute_create_spec`,
  `test_execute_create_spec_error_path`)
- `handle_create_phase` (and its tests: `test_execute_create_phase`,
  `test_execute_create_phase_error_path`)

Verify: `cargo check` passes.

#### Phase 4 - Delete dead tests for integration executor and errors

From `src/tests/integration/executor.rs`:
- Delete `test_coordinator_action_creates_plan_via_executor` entirely (only tests `CreatePlan`)
- Rewrite `test_coordinator_creates_full_hierarchy_via_executor`: use `create_test_hierarchy`
  fixture to build Plan/Spec/Phase, then keep only the `CreateWork` step (line ~117) and the
  subsequent Work record assertions. This preserves coverage of the live `handle_create_work`.

Delete from `src/tests/integration/errors.rs`:
- `test_create_spec_with_invalid_plan_id_returns_error` (uses `CreateSpec`)

Verify: `otto ci` passes.

#### Phase 5 - Delete dead coordinator functions and their tests

From `src/agents/coordinator.rs`, delete:
- `build_generation_footer` (lines 408-647, the entire function body)
- `query_learnings_for_level` (line 370)
- `find_pending_draft_for_validation` (line 782)

From `src/agents/coordinator/tests.rs`, delete the nine `test_build_generation_footer_*`
tests.

Remove now-unused imports from coordinator.rs: `build_phase_prompt`, `build_plan_prompt`,
`build_spec_prompt`, `GenerationLevel`. Keep `build_work_prompt` (used in `ActivatePhase`).

Verify: `cargo check` passes.

#### Phase 6 - Delete dead generation functions, dead prompt files, and `generation/tests.rs`

From `src/agents/generation.rs`, delete the 15 dead functions identified above.
Keep the 6 live functions and `build_work_prompt`.

Delete the three dead generation prompt files (use `rkvr rmrf`):
- `prompts/generation-plan.pmt`
- `prompts/generation-spec.pmt`
- `prompts/generation-phase.pmt`

Keep `prompts/generation-work.pmt`.

From `src/prompts.rs`:
- Remove the `generation_plan`, `generation_spec`, `generation_phase` fields from `PromptStore`
- Remove the `DEFAULT_GENERATION_PLAN`, `DEFAULT_GENERATION_SPEC`, `DEFAULT_GENERATION_PHASE`
  constants and their `include_str!` calls
- Remove the corresponding `load()` calls from the init path
- Delete `test_generation_plan_pmt_content`, `test_generation_spec_pmt_content`,
  `test_generation_phase_pmt_content` (the prompt files they test no longer exist)
- Keep the `generation_work` field and its test

Before deleting `src/agents/generation/tests.rs`, migrate these 12 tests for live
functions into a `#[cfg(test)] mod tests` block inside `generation.rs`:

| Test | Live function |
|------|---------------|
| `test_find_active_plan_none/some/skips_draft` | `find_active_plan` |
| `test_find_active_specs_for_plan` | `find_active_specs_for_plan` |
| `test_find_active_phases_for_spec_sorted` | `find_active_phases_for_spec` |
| `test_find_works_for_parent`, `test_find_works_for_parent_ordering` | `find_works_for_parent` |
| `test_is_phase_complete_*` (5 tests) | `is_phase_complete` |

Then delete `src/agents/generation/tests.rs` entirely (remaining 63 tests all cover dead
functions). The external `tests.rs` file can be deleted once the 12 tests are migrated in.

Verify: `otto ci` passes (final gate).

---

## Alternatives Considered

### Alternative 1: Keep the dead functions but add a compile gate

Add a `cfg(dead_generation)` feature flag and hide everything behind it, so the
functions exist for debugging but don't appear in normal builds.

**Pros:** Recoverable if the Decomposer path is abandoned.

**Cons:** Feature-flagged dead code is still dead code, just harder to find. The
`coordinator.pmt` hallucination risk remains. The tests still run by default.

**Why not chosen:** The Decomposer IS the architecture. There is no scenario where
the Coordinator resumes generating Specs. If that changes, the code can be recovered
from git history.

### Alternative 2: Delete only the coordinator functions, leave `generation.rs`

Keep `generation.rs` intact with `#[allow(dead_code)]` on the dead functions.

**Pros:** Less churn.

**Cons:** The `coordinator.pmt` hallucination risk remains (the functions inform
what actions are plausible to the author, who might re-add them to the prompt).
The test suite still runs 75 tests on dead code.

**Why not chosen:** Half-measures compound over time.

---

## Technical Considerations

### Dependencies

No new dependencies. All deletions.

### Testing Strategy

Each phase ends with `cargo check`. Phases 4 and 6 end with `otto ci`. No phase
leaves the build broken. The live functions retain coverage via coordinator
integration tests.

### Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| A `generation.rs` function marked dead is actually called from a path not found by grep | Low | High | `cargo check` will catch it - unused function calls produce warnings; missing functions produce errors |
| `build_work_prompt` is deleted with the dead functions | High | High | It is listed in the LIVE column; Phase 6 must explicitly preserve it |
| `AgentAction::CreateWork` serde tests are deleted with the dead variant tests | Medium | Low | Check that `test_agent_action_create_work_serde` is NOT deleted in Phase 2 |
| The coordinator prompt removal breaks an existing test that asserts on prompt content | Low | Low | `cargo test` will surface it |
| A generation test that tests a LIVE function (e.g. `find_active_plan`) is deleted in Phase 6 | Medium | Medium | Before deleting `generation/tests.rs`, identify any tests for live functions and move them to a `#[cfg(test)]` block inside `generation.rs` |
| `test_coordinator_pmt_identity` passes after Phase 1 without adding the `!contains` guards | High | High | Phase 1 explicitly requires adding the negative assertions, not just removing the positive ones |
| `generation-work.pmt` is deleted alongside the other three generation prompt files | High | High | It is in the LIVE column; `build_work_prompt` at coordinator.rs:1135 depends on it |

---

## Open Questions

- [ ] **`find_active_plan` tests**: `generation/tests.rs` contains unit tests for
  live functions (`find_active_plan`, `find_works_for_parent`, `is_phase_complete`,
  `find_active_specs_for_plan`). These should be moved to a `#[cfg(test)] mod tests`
  block inside `generation.rs` before the external file is deleted. Confirm which
  tests to migrate.
- [ ] **`double_write_old_records`**: Still in `doc.rs`. Once this is removed, the
  live generation query functions (`find_active_plan`, etc.) will find nothing
  in the old stores. The coordinator's SLA and phase completion logic will need to
  be migrated to read from Docs directly. This is a follow-on design doc, not in scope here.

---

## References

- `docs/design/2026-04-04-delete-non-doc-paths-retool-e2e.md` - completed deletion of IPC entry paths
- `src/agents/coordinator.rs:408` - `build_generation_footer` (dead, `#[allow(dead_code)]`)
- `src/agents/generation.rs` - 1037 lines, 15 dead functions + 6 live + `build_work_prompt`
- `src/agents/generation/tests.rs` - 1415 lines, all testing dead code
- `prompts/coordinator.pmt:27-29` - the three hallucination-risk action definitions
