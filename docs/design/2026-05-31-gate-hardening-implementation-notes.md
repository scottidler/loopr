# Implementation Notes: First-Gate Hardening + Failure-Path Tests

Companion to [2026-05-31-gate-hardening.md](2026-05-31-gate-hardening.md).
Append-only. One section per phase, four buckets each.

## Phase F: ScriptedLlm prompt-content keying

### Design decisions
- Keyed match runs against a **haystack = system prompt + all `MessageContent::Text`
  blocks** (for `complete_free`) or **system + user** (for `complete_with_tool`)
  — `crates/llm/src/stub.rs`. Rationale: implementer and reviewer both call
  `complete_free` with `model: None`, so model-routing alone cannot disambiguate
  them; the test author picks a needle substring unique to the (role, Work) pair.
- **Keyed-first, model-FIFO fallback** — a `complete_*` call first scans the keyed
  list for the first entry whose needle is a substring of the haystack; only if
  none matches does it fall back to the existing per-model FIFO queue. Preserves
  every existing caller (they queue no keyed entries, so they hit the FIFO path
  unchanged).
- Keyed store is a `Vec<(String, Result<T, LlmError>)>` (insertion-ordered, first
  match wins on substring), not a `HashMap` — needles are substrings, not exact
  keys, so a map keyed by needle would not help selection.

### Deviations
- None.

### Tradeoffs
- New tests appended to the **existing inline `#[cfg(test)] mod tests`** in
  `stub.rs` rather than extracting to a sibling `stub/tests.rs`. The repo rule
  prefers sibling test files, but its own text says the inline→sibling migration
  is "a tree-wide mechanical pass, never mixed into a feature." Extracting here
  would mix an unrelated refactor into this phase, so the inline module is kept.

### Open questions
- None.

## Phase A: Director event-driven wake-up

### Design decisions
- `DaemonContext::wake_director(&self, plan_id)` (async) reads
  `operator_notifies` and `notify_one()`s the Plan's `Notify`; a miss is
  benign. Fired AFTER the triggering persist (ordering invariant) at every
  recoverable Director-actionable transition:
  - `spawn_reviewer_for_bundle`: Accept (Bundle Reviewed — the measured
    ~16.6s case), ChangeRequested/Reject (Work Blocked), and the
    Escalation/reviewer-error arms (Work Blocked).
  - `spawn_implementer_for_work`: worktree-create-fail (Work Blocked) and a
    single post-match guard `if work.status == Blocked` covering the
    escalation/error arms.
  - `spawn_integrator_for_bundle`: ValidationFailed and terminal-error arms
    (Work Blocked), inline (the Ok arm moves `work`, so no post-match guard).

### Deviations
- Work -> Ready (sibling unblock) does NOT wake the Director:
  `promote_unblocked_siblings` dispatches the Implementer directly, so the
  Director has nothing to do. Same for the initial Pending -> Ready.
- `block_dependent_siblings` deliberately does NOT wake: those Blocks are
  terminal-by-irrecoverable-dependency, not recovery candidates — waking
  would invite a wasted Director override.

### Tradeoffs
- Wake at the ctx-method call sites rather than inside the free function
  `transition_and_persist_work`, which has no `ctx`/notify access. Threading
  the notify map through its ~15 call sites would be far more invasive than
  the handful of `self.wake_director(...)` calls at the actionable sites.

### Open questions
- DAG consolidation (deferred, user decision 2026-05-31): the ready-set /
  dependents / cycle-detect logic is hand-rolled in four places (dep-gate
  partition, `promote_unblocked_siblings`, `block_dependent_siblings`,
  decomposer `cycles.rs`). A future design doc should put a `petgraph`/daggy
  -backed `WorkGraph` underneath, answering "what is runnable (in parallel
  up to a jobs limit) vs. blocked"; loopr keeps only the FSM/retry/Director
  policy *around* that answer. Not part of this gate-hardening pass.

## Phase B: async plan.create ACK

### Design decisions
- The decompose chain was extracted into a free function
  `decompose_and_dispatch(ctx, plan, request_id)` in
  `crates/loopr/src/transport/handler.rs` carrying its own
  `#[tracing::instrument(fields(request_id, plan_id))]` — `tokio::spawn`
  detaches the task from the handler's span, so the span must be re-opened
  on the task or the decompose logs lose `plan_id`.
- New task pool `DaemonContext::plan_create_tasks: Mutex<JoinSet<()>>`,
  drained FIRST in `serve_core`'s shutdown sequence
  (`drain_plan_create_tasks`, `PLAN_CREATE_DRAIN_TIMEOUT_SECS = 30`),
  ahead of implementer/reviewer/director/work_spawner/integrator. It is
  the root of the spawn DAG.
- The spawned task guards on `shutting_down` at entry (belt-and-suspenders,
  matching the other spawn-task bodies).

### Deviations
- None. The ACK boundary, payload (`PlanCreateResult { plan }` unchanged),
  and decompose-failure semantics match the design doc.

### Tradeoffs
- `PLAN_CREATE_DRAIN_TIMEOUT_SECS = 30` mirrors the LLM-bearing pools
  (implementer/director) rather than the sub-second work_spawner budget,
  since the task's dominant cost is the decompose LLM call.
- Updated `plan_create_with_failing_llm_still_persists_plan_and_leaves_works_empty`
  to drain `plan_create_tasks` before the works-empty assertion, so it
  tests "decompose ran and failed" rather than "async hasn't started."
  `plan_create_persists_and_returns_plan` needed no change (it only
  asserts the synchronously-persisted Plan).

### Open questions
- None. (Surfacing post-ACK work counts to the user is the existing
  `loopr works <plan-id>`; no new status verb added, per the design doc.)

## Phase E: controlled failure-path tests

### Design decisions
- `crates/loopr/tests/failure_paths.rs` is self-contained — it duplicates
  the small JSONL readers (`load_works`/`load_bundles`/`load_ticks`/
  `read_jsonl`/`HasId`) from `stage_8_plan_to_tick.rs` rather than sharing
  them, so the green `stage_8` is not touched.
- `FailureDirectorAwareLlm` wraps `ScriptedLlm`: Director calls
  (model = `Some("claude-opus-4-7")`) emit `accept_bundle` for a Reviewed
  Bundle, else `override_work {Ready}` for a Blocked Work, else `done`.
  Implementer/Reviewer calls (model = None) forward to the inner stub.
  Intercepting Director calls in the wrapper is what keeps the keyed free
  entries from being consumed by the Director's own prompt (which echoes
  Work titles / filenames).
- `wait_for_tick` here does **not** fast-fail on `Blocked` (unlike
  `stage_8`): `Blocked` is the transient state during recovery.
- Keying convention: for a given filename key, the Implementer response is
  queued before the Reviewer response; `take_keyed` is first-match-wins by
  insertion order, and the Implementer call is causally earlier than the
  Reviewer call, so each binds correctly. Draining protects the dependent
  Work: by the time Work B runs, the "A.md" entries are gone, so B's
  prompt (which references its dependency "Add A.md") falls through to the
  "B.md" entries.

### Deviations
- None. All three scenarios from the design doc are implemented:
  `reject_then_recover_reaches_tick` (scenario 2, FIFO),
  `multi_work_dag_unblocks_and_completes` (scenario 1, keyed),
  `mid_dag_failure_recovers_then_unblocks_downstream` (scenario 3, keyed).

### Tradeoffs
- JSONL-reader duplication with `stage_8` accepted (see above) over a
  shared `common/records.rs`, to avoid editing a passing test in this
  phase. A future cleanup could hoist them into `common/`.

### Open questions
- None.
