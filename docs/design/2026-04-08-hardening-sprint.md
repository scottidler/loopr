# Design Document: v3 Hardening Sprint

**Author:** Scott A. Idler
**Date:** 2026-04-08
**Status:** In Review
**Review Passes Completed:** 5/5

## Summary

Five targeted improvements to address the remaining failure modes observed in E2E runs and
accumulated technical debt. Covers: parallelism hardening (coordinator prompt cap removal and
max_pool/worker_pool_size alignment), integrator self-heal for structural merge conflicts,
domain model phase 7 (children frontmatter links), and prompt SSOT refactor (eliminate magic
strings duplicated across .pmt files and Rust source).

## Problem Statement

### Background

v0.1.105 fixed the executor death-loop (worker-spawned implementers were invisible to the
reconciler because their JoinHandle was never registered). With that fix, implementers can
now run concurrently as intended. However, several issues still limit throughput and
reliability.

### Problems

**P1 - Soft parallelism cap in coordinator prompt**
Line 80 of `coordinator.pmt` says "Keep 2-6 implementers active." The coordinator LLM
reads this as a ceiling and a floor. With 10+ work items, capping at 6 artificially
serializes the pipeline. More critically, the "2" minimum causes the coordinator to hold
back assignments until 2 are active, wasting time when only 1 is available.

**P2 - max_pool decoupled from worker_pool_size**
`worker_pool_size` controls how many worker worker tasks poll for work (default: `auto` = nproc).
`max_pool` on `AgentRoleConfig` is a separate hard cap on concurrent implementer sessions
(default: 6). With `worker_pool_size: auto` on a 16-core machine, 16 workers poll but only 6
sessions are ever allowed. The two knobs are not aware of each other and must be set
independently.

**P3 - No self-heal for structural merge conflicts**
When two work items produce incompatible implementations of the same file (e.g. two specs
both define `database.py` with different function signatures), the integrator aborts the
merge and resets the conflicting works to Ready for retry. A simple retry makes the same
conflict again. The system has no path to detect structural conflicts and escalate them to
the decomposer for respecification.

**P4 - Domain model phase 7 incomplete**
`update_parent_children()` exists in `src/domain/markdown.rs` (line 260) and is fully
implemented. It is never called. Plan, Spec, and Phase documents do not include a `children:`
frontmatter list, making the doc hierarchy non-navigable from the top down.

**P5 - Prompt magic strings duplicated**
Section headers, FSM state names, and criteria labels are hardcoded in `.pmt` files AND in
the Rust source that parses responses. When a string changes, it must be updated in 3-4
places. This caused a real 15-minute E2E timeout (casing mismatch: "ready" vs "Ready") and
required touching multiple files when reviewer criteria section names changed.

### Goals

- Remove the artificial 6-implementer ceiling from coordinator behavior
- Align `max_pool` with `worker_pool_size` so a single config value controls concurrency
- Detect structural merge conflicts and escalate to redecomposition
- Wire up `update_parent_children()` so doc hierarchy is fully linked
- Establish a single source of truth for prompt section headers and criteria names

### Non-Goals

- Full prompt template engine (tera/handlebars) - extend existing interpolation mechanism
- Changing FSM semantics or adding new Work statuses
- Rewriting the integrator merge strategy
- Parallelism across Specs (sequential spec execution is intentional)

## Proposed Solution

### Phase 1 - Parallelism Hardening

**1a. Coordinator prompt line 80**
Replace:
```
- Keep 2-6 implementers active. Assign to Ready Works with met dependencies first.
```
With:
```
- Assign ALL Ready Works immediately. Do not wait for earlier Works to finish before
  assigning new ones. Workers pull work automatically - your job is to keep the queue full.
```

**1b. max_pool default alignment**
Treat `max_pool: 0` (or sentinel `u32::MAX`) as "derive from worker_pool_size". In
`run_single_work` (the only call site that checks `max_pool`), resolve the effective cap:

```rust
const UNLIMITED_POOL: u32 = u32::MAX;

let effective_max = if implementer_config.max_pool == UNLIMITED_POOL {
    stores.config.agents.worker_pool_size.resolve() as usize
} else {
    implementer_config.max_pool as usize
};
```

Change `default_implementer()` to use `UNLIMITED_POOL` so `worker_pool_size` becomes the
single knob by default. Users who want a hard cap can still set `max-pool:` explicitly in
config. This avoids breaking existing config files (explicit `max-pool: 6` still works)
while making the default behavior correct.

Note: `u32::MAX` is safe as a sentinel - the worker pool worker tasks are the actual
concurrency bottleneck before `max_pool` matters. Document in config comments that an
absent/unset `max-pool` = unlimited (bounded by `worker-pool-size`), and any explicit
value = hard cap. Users who explicitly set `max-pool: 4294967295` in YAML will get
"unlimited" behavior, which is correct intent.

### Phase 2 - Self-Heal for Structural Merge Conflicts

The integrator already has `reset_work_after_bundle_rejection`. The gap is distinguishing
retryable conflicts (stale base, replay failure) from structural conflicts (two bundles
touch the same file with incompatible changes).

**Conflict classification**
After `git merge --no-ff` fails, inspect the conflict markers:
1. Run `git diff --name-only --diff-filter=U` to get unresolved files
2. Compare those files against `bundle.files` for all bundles in the tick
3. If two or more bundles claim the same file in their `files` list, it is a structural
   conflict. Otherwise it is retryable.

**Structural conflict escalation - Option A (Learning poll)**
For structural conflicts:
1. `git merge --abort` (already done)
2. Reject all conflicting bundles (already done)
3. Reset conflicting works to `Ready` (existing `reset_work_after_bundle_rejection`, unchanged)
4. Create a Learning via `learning.create` with structured content:
   ```
   STRUCTURAL CONFLICT: Works [wk-abc, wk-def] both modified [database.py].
   Their implementations are incompatible. The phase needs redecomposition - the
   coordinator should Abandon these works and use the decomposer to regenerate the
   phase with explicit file ownership declared per work item.
   ```
5. The coordinator's existing state summary surfaces Learnings. On its next iteration in
   `Executing` state, it reads this Learning, Abandons the works, and invokes the decomposer
   to regenerate the phase.

This requires a small addition to coordinator.pmt: instruct the coordinator that when it
sees a Learning containing "STRUCTURAL CONFLICT", it must Abandon the named works and
redecompose the phase (not just retry). No new IPC surface required.

Note: do NOT transition conflicting works to `Blocked` - `Blocked` is a dependency-wait
state. Resetting to `Ready` then creating the Learning is the correct sequence; the
coordinator will Abandon them on its next pass before they get picked up again by a worker.

**Edge case - race with worker re-pickup:** Between the integrator resetting a conflicting
work to `Ready` and the coordinator reading the structural-conflict Learning, a worker could
claim and start the same work again. The coordinator must handle this: when it sees a
structural-conflict Learning and the named work is `InProgress`, it should use
`override_work` to force it to `Abandoned` (override is valid on InProgress). Add this
case to the coordinator prompt's structural-conflict handling instruction.

**Edge case - unresolvable conflict files:** If `git diff --name-only --diff-filter=U`
returns empty (binary conflict, submodule conflict, etc.), fall back to the retryable path.
Log a warning but do not escalate. Structural classification requires confirmed file overlap.

### Phase 3 - Domain Model Phase 7

Wire up `update_parent_children()` at two call sites:

1. In `persist_hierarchy()` (decomposer) - after each child doc is persisted, call
   `update_parent_children(repo_path, parent_id, child_id, child_title)` to add the child
   link to the parent's frontmatter.

2. In `handle_work_create()` (IPC handler) - after a Work is persisted to disk, call
   `update_parent_children(repo_path, work.parent_id, work.id, work.title)`.

The function signature and implementation already exist. This is purely a wiring task.

Update tests to verify that after creating children, the parent doc's frontmatter contains
the expected `children:` block.

### Phase 4 - Prompt SSOT Refactor

Extend the existing `interpolate_status_values()` in `src/prompts.rs` to also inject
section header constants. Define the constants in `prompts.rs` and interpolate via
`{placeholder}` tokens in `.pmt` files.

The most pervasive offender, confirmed by grep: `"Acceptance Criteria"` appears in:
- 4 domain struct `body_markdown()` methods (plan, spec, phase, work)
- `src/agents/executor.rs` (pre-flight check prompt)
- `src/prompts.rs:361` (reviewer criteria extraction)
- `prompts/reviewer.pmt`, `prompts/decompose/validate.pmt`, `prompts/decompose/work.pmt`
- 7 test assertions

**Constants to define in `src/prompts.rs`:**
```rust
pub const SECTION_AC: &str = "Acceptance Criteria";
pub const SECTION_OVERVIEW: &str = "Overview";
pub const SECTION_IMPLEMENTATION: &str = "Implementation Notes";
```

These are section *names* (without `##`), consistent with how `strip_markdown_section`
takes the name without the prefix. Call sites that write `## Acceptance Criteria` use
`format!("## {}", SECTION_AC)`.

**Interpolation tokens** are NOT needed for these - they are referenced via the Rust
constant, not via `.pmt` template interpolation. `.pmt` files that hardcode section
headers get updated to the same string, and tests reference the constant.

**Scope boundary:**
- In scope: strings parsed in Rust AND embedded in prompts (`"Acceptance Criteria"`)
- Out of scope: prose only in prompts, never parsed (e.g. "Do NOT retry the same action")

## Alternatives Considered

### Alternative: Raise max_pool to 32 (hardcode)
- **Pros:** Simple, one-line change
- **Cons:** Still decoupled from worker_pool_size; the two values drift independently
- **Why not chosen:** Doesn't solve the underlying coupling problem

### Alternative: Full template engine for SSOT (tera/handlebars)
- **Pros:** Full-featured, handles conditionals and loops
- **Cons:** New dependency, over-engineered for the problem (only ~10 strings to centralize)
- **Why not chosen:** Extending existing interpolation is sufficient and zero-dependency

### Alternative: New Work status `Conflicted` for self-heal
- **Pros:** Explicit state machine representation of the conflict condition
- **Cons:** Adds FSM complexity, requires new transition rules, tests, and coordinator prompt
  instructions for a state that should be transient
- **Why not chosen:** Learning-based escalation achieves the same outcome via existing
  coordinator retry infrastructure

### Alternative: Integrator calls decomposer directly (bypassing coordinator)
- **Pros:** Faster loop, no coordinator involvement
- **Cons:** Violates the pipeline hierarchy. The coordinator owns the Plan/Spec/Phase/Work
  decomposition contract. Integrator should not decompose.
- **Why not chosen:** Architectural boundary violation

## Technical Considerations

### Dependencies

- Phase 1: `prompts/coordinator.pmt`, `src/config.rs`, `src/agents/executor.rs`
- Phase 2: `src/agents/integrator.rs`, `prompts/coordinator.pmt`
- Phase 3: `src/decomposer/`, `src/daemon/handlers/work.rs`, `src/domain/markdown.rs`
- Phase 4: `src/prompts.rs`, `prompts/*.pmt`, `tests/chat_prompts.rs`,
  `src/agents/coordinator/tests.rs`, `src/agents/reviewer*.rs`

### Performance

- Phase 3 adds a file read + write per child creation. `update_parent_children` is O(n)
  over the parent doc's lines. For typical doc sizes (<200 lines) this is negligible.
- Phase 4 adds string allocations at prompt load time (startup, one-time). No runtime cost.

### Security

No external inputs involved in any phase. No security implications.

### Testing Strategy

- Phase 1: E2E run with python-api target. Monitor that more than 6 implementers become
  active concurrently on a work set larger than 6.
- Phase 2: Unit tests for conflict classification (`structural` vs `retryable`). Integration
  test that creates two bundles with overlapping `files`, triggers merge failure, and verifies
  Learning is created and coordinator receives redecompose signal.
- Phase 3: Unit tests verifying `update_parent_children` is called after create. Read parent
  doc and assert `children:` list is populated.
- Phase 4: Static test: for every string constant defined in `prompts.rs`, assert that no
  test file contains the same literal string (enforces SSOT at test layer).

### Rollout Plan

Ship in this order (risk-ascending, not numbered by phase):
1. **Prompt hygiene** (Phase 1a + coordinator.pmt structural-conflict section): prompt-only, zero risk
2. **Parallelism config** (Phase 1b): one-line default change in `config.rs`, one-line check change in `executor.rs`
3. **Domain model phase 7** (Phase 3): mechanical wiring of existing function
4. **Prompt SSOT** (Phase 4): refactor only, no behavior change, verify with `otto ci`
5. **Self-heal** (Phase 2): new code path in integrator, new conflict classification logic - validate with E2E after all others are stable

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Higher parallelism hits API rate limits | Med | Med | Per-agent `session_timeout_secs` and `max_tokens` provide natural throttling. Monitor E2E logs. |
| Self-heal escalation creates infinite redecompose loop | Low | High | Add a counter to the structural-conflict Learning (e.g. "redecompose attempt 1 of 3"); coordinator prompt instructs it to emit `need_help` after N attempts rather than decomposing again |
| update_parent_children corrupts frontmatter on malformed docs | Low | Med | Function already handles missing frontmatter gracefully (warns, no-ops). No change needed. |
| Prompt SSOT interpolation misses a call site | Med | Low | Add CI test that scans .pmt files for known hardcoded strings that should be tokenized |

## Open Questions

- [ ] Phase 2: Confirm the Learning content format is sufficient for the coordinator to
      reliably detect and act on structural conflicts, or does the coordinator prompt need
      an explicit pattern match instruction added?
- [ ] Phase 4: Are `SECTION_OVERVIEW` and `SECTION_IMPLEMENTATION` actually duplicated
      between Rust and prompts, or only `SECTION_AC`? Audit before implementing.

## References

- `docs/design/2026-04-07-domain-model-cleanup.md` - phase 7 context
- `src/agents/integrator.rs:559` - merge failure handling
- `src/domain/markdown.rs:260` - `update_parent_children` implementation
- `src/prompts.rs:68` - `interpolate_status_values` - extension point for Phase 4
- `src/config.rs:308` - `WorkerPoolSize` enum
- `src/config.rs:394` - `AgentRoleConfig.max_pool`
