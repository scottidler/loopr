# Design Document: Strategy-Primitive Wiring Round 4 Follow-Up

**Author:** Scott Idler
**Date:** 2026-04-17
**Status:** Approved (pending implementation)
**Review Passes Completed:** 5/5 + Architect Round 2

## Summary

Round 3 of the Architect audit closed out Findings 1 (event casing) and 2
(idempotency classification) with narrow fixes, deferred Finding 3
(typed-struct handler sweep) as hardening, and validated Finding 4 (E2E
run) which surfaced two pre-existing strategy-catalog bugs. A third bug
emerged during this document's correctness review: the reviewer
verdict → retry pipeline is non-functional because the trigger lacks
scope and the event payload lacks the required fields. Architect
Round 2 review of the draft uncovered two more blockers — the
integration pipeline is structurally dead because its trigger queries a
non-existent field, and the `escalate` primitive hardcodes a call to
a removed IPC endpoint that 4+ strategies depend on. This document
rolls the remaining deferred work plus the five newly-surfaced bugs
into a single cohesive plan so the next implementation pass leaves the
runtime in a state where the full pipeline executes end-to-end.

## Locked Decisions (from Architect Round 2 + v4-vision alignment)

All contested design calls resolved. Implementation follows:

- **Escalate repair:** rename IPC to `director.escalate`; implement
  handler in `src/daemon/handlers/director.rs`. The `escalate`
  primitive's call site in `src/primitive/catalog/sweep.rs:278`
  updated; all 4 dependent strategies recovered.
- **Guard signature:** unchanged. Strategies re-scoped at trigger
  layer (Phase 2 Option A).
- **Reviewer event:** new `AgentEvent::ReviewerVerdict` variant.
- **Bug B `restart-director-on-state`:** **kept, not deleted.**
  Vision Principle 8 requires the level-triggered fallback. Extend
  `check-threshold` to support `collection: session` with aggregate
  semantics: count Director `AgentSession` records attached to the
  scope_id (plan) with status `Failed`. No new per-session field.
- **`deny_unknown_fields`:** blanket apply to all 15 handlers; fix
  any caller drift that surfaces.
- **Handler decomposition:** decompose `work.rs` (2085 lines > 1500
  threshold per `rules/rust.md`) during its sweep commit; leave
  others.
- **Event rename compat:** hard rename; no `serde(alias)` window
  (v4 is pre-release, clean-break).
- **Finding 2 primitives:** restore to `GuardRequired` with the two
  new guards registered in Phase 5.
- **Ghost integration pipeline:** fix `bundles-ready-for-integration`
  trigger by implementing a real `has-accepted-bundles` state query
  that counts Bundles with `plan_id == scope_id AND status ==
  Accepted`. Without this fix, `integrate-tick` and the entire
  integration pipeline are dead code.

## Problem Statement

### Background

Commit `ee6bdac` (v0.1.140) shipped the strategy-primitive wiring sweep
per `docs/design/2026-04-17-strategy-primitive-wiring.md`. The Architect
reviewed and flagged four issues; three were addressed in v0.1.141 and
one was deferred. The follow-up E2E run (Finding 4) confirmed the
event-casing fix was correct but uncovered a decomposition infinite
loop and a dead recovery strategy that together prevent the pipeline
from completing any real task.

### Problem

Eight independent but related issues remain open:

| # | Category | Origin | Nature |
|---|----------|--------|--------|
| 1 | Event-naming drift | Finding 1 residual | Correctness hardening |
| 2 | Fake-idempotency gate bypass | Finding 2 residual | Correctness hardening |
| 3 | Dynamic-key handler params | Finding 3 deferred | Correctness hardening |
| A | Decomposer infinite loop | Finding 4 E2E | **Blocks pipeline** |
| B | Broken supervision strategy | Finding 4 E2E | Noise + systemic recovery path broken |
| C | Reviewer verdict path broken | Pass-2 discovery | **Blocks pipeline** |
| D | Ghost integration trigger | Architect Round 2 | **Blocks pipeline** |
| E | Systemic escalate primitive failure | Architect Round 2 | **Blocks all error recovery** |

Details:

**1. Finding 1 residuals — snake/kebab drift in DaemonEvent data.**
Finding 1 flipped agent-event payload keys to kebab-case but other
`DaemonEvent` constructors still emit snake_case data fields
(`learning_id`, `idle_secs`, `merged_bundle_ids`, `bundle_work_id`,
`latest_tick_id`). Several event-name literals also mix casing
(`bundle.rejected_stale`, `learning.policy_contradicted`,
`director.mode_changed`, `director.stall_detected`,
`tick.validation_failed`).

**2. Finding 2 residual — NonIdempotent vs GuardRequired.** Round 3
settled on `NonIdempotent` for `retry-work`,
`combine-conflicting-works`, and `integrate-tick`. The Architect's
language ("Fix the invoking strategies by adding real guard
clauses") suggests they prefer `GuardRequired` with new guards. Real
guards exist or can be cheaply added for all three invocations once
Bug C realigns the reviewer-verdict strategies.

**3. Finding 3 — typed-struct sweep.** Only 4 of ~19 handlers adopted
`#[serde(rename_all = "kebab-case", deny_unknown_fields)]` typed
params structs. The other 15 still use `params.get("foo-bar")`
string-literal lookups. A misspelled key becomes a silent `None` at
runtime instead of a schema-level parse failure.

**A. Brief-mode plan-decomposable loop.** The `plan-decomposable`
composite fires when `plan-is-active AND plan-active-no-specs`.
Brief-mode plans (per `resources/decompose/roles/brief.yml`) produce
Works directly and never gain Specs, so `plan-active-no-specs` stays
true forever. The strategy's `no-active-sessions` guard only gates
while a prior decomposer is running. In the E2E run this produced 31
duplicate Works in 10 minutes, 0 implementers spawned.

**B. Dead restart-director-on-state strategy.** Calls
`check-threshold` with `collection: session` (unsupported:
`src/primitive/catalog/reconcile.rs:310` bails). On-failure calls the
`escalate` primitive which requests `coordinator.escalate` — an IPC
method that no longer exists after the coordinator → director
rename. Two WARNs per engine tick, masking real issues in logs.

**C. Reviewer verdict → retry path non-functional.** Pass 2
correctness review uncovered that `reviewer-rejected` and
`reviewer-approved` triggers in
`resources/engine/triggers/agent-lifecycle.yml` have no `scope:`
field, so `eval_event` returns empty `scope_ids`. `AgentEvent`
payloads emitted by the reviewer session carry only
`session-id`/`status`, not `bundle-id` or `work-id`. The
`handle-rejected-bundle` strategy's first step uses
`$trigger.scope-id` (empty) and its second step uses
`$trigger.event.work-id` (absent). Reviewer verdicts have no
automation path today — any reviewer cycle stalls.

**D. Ghost integration trigger.**
`resources/engine/triggers/engine.yml:12` defines
`bundles-ready-for-integration` as a `field-equals` state query
against `field: has-accepted-bundles` on the `Plan` collection. The
`Plan` struct (`src/domain/plan.rs`) has no such field, and
`field-equals` performs direct JSON key lookup. The query can never
evaluate true. `integrate-accepted-bundles` never fires. No tick is
ever created, no bundle ever merged. The file even admits the gap
in a comment: *"uses field-equals as a placeholder; a dedicated
has-accepted-bundles state query will replace this in Phase 3"* —
but the replacement never landed.

**E. Systemic escalate primitive failure.**
`src/primitive/catalog/sweep.rs:278` unconditionally calls
`ctx.bridge.request("coordinator.escalate", params)`. The
`coordinator.escalate` IPC endpoint was removed during the
coordinator → director rename. Every strategy that wires
`escalate` as its `on-failure` path currently bails at runtime.
Four strategies depend on this: `abandon-ratio-escalation`
(recovery.yml), `merge-integration-to-main-on-goal`
(git-lifecycle.yml), a feedback strategy (feedback.yml),
`restart-director-on-event` (supervision.yml). The entire
error-recovery layer is non-functional.

### Goals

- Finish the event-naming normalization: no mixed snake/kebab in any
  `DaemonEvent` data payload.
- Enrich the reviewer-session status event so `bundle-id` and
  `work-id` are on the event bus when a reviewer verdict lands.
- Restore `GuardRequired` for the three primitives the Architect
  flagged, with real guards registered in the
  `GuardConditionRegistry`.
- Convert the remaining ~15 handlers to typed params structs, one
  handler per commit, each validated by `otto ci`.
- Fix Bug A so brief-mode plans decompose exactly once.
- Fix Bug B by extending `check-threshold` to support
  `collection: session` with aggregate-failed-session semantics;
  keep the strategy intact per Vision Principle 8.
- Fix Bug C so reviewer verdicts drive the retry/reject pipeline
  automatically.
- Fix Bug D so the integration pipeline actually fires when a plan
  has accepted bundles ready for integration.
- Fix Bug E by renaming the IPC endpoint from `coordinator.escalate`
  to `director.escalate` and implementing the corresponding handler,
  restoring the `on-failure` recovery path for all 4 dependent
  strategies.
- Leave the E2E `rust-version` test reaching goal completion
  (exit 0), proving the full pipeline executes end-to-end.

### Non-Goals

- No new features. This round is strictly remediation.
- No changes to IPC framing or the FSM rule set. (Bug C adds a new
  `AgentEvent` variant, which is an additive protocol evolution, not
  a framing change.)
- No redesign of the guard registry signature. We register new
  guards against the existing `(ctx, collection, id) -> bool` shape.
  Strategies that need cross-scope resolution are re-scoped at the
  trigger layer (Phase 2 Option A), not by extending the guard API.
- No changes to the decomposer role configs. Brief mode stays brief;
  the fix is at the trigger-composite layer.
- No decomposition of the large handler files (`work.rs` at 2085
  lines, `integrator.rs` at 1295 lines, etc.) during the typed-struct
  sweep. That is a separate rule-driven refactor for another round.

## Proposed Solution

### Overview

Eight sequential phases, chosen so blocking issues land before
hardening:

| Phase | Problem | Why this order |
|-------|---------|----------------|
| 1 | A: decomposer loop + D: ghost integration trigger | Both are pipeline blockers touching trigger composites; land together |
| 2 | C: reviewer verdict path | Must land before Phase 4 (Finding 2 needs the work-scoped retry strategy Phase 2 creates) |
| 3 | E: escalate primitive IPC repair | Unblocks all 4 dependent strategies' on-failure paths; required before any Phase 4 guard failure path can recover |
| 4 | B: supervision strategy + check-threshold session support | Extends check-threshold; keeps the level-triggered fallback per Vision Principle 8 |
| 5 | Finding 2 residual | Now possible: all invoking strategies are scope-aligned after Phase 2, escalate works after Phase 3 |
| 6 | Finding 1 residual | Normalize the remaining snake keys and mixed event names |
| 7 | Finding 3 typed-struct sweep | Largest volume (15 commits) but least risk; independent of other phases |

Phase 8 is a validation step — E2E reruns to confirm the pipeline
executes end-to-end.

### Architecture

No architectural changes. All work happens inside existing modules.

```
src/
  agents/
    event.rs                 ← Phase 2 (new ReviewerVerdict variant)
    executor/lifecycle.rs    ← Phase 2 (reviewer event emission)
  daemon/handlers/
    director.rs              ← Phase 3 (new director.escalate handler)
    <15 handlers>            ← Phase 7 (typed-struct sweep, 1 commit each)
  ipc/protocol.rs            ← Phase 6 (remaining snake-key cleanup)
  primitive/catalog/
    reconcile.rs             ← Phase 4 (check-threshold session support)
    sweep.rs                 ← Phase 3 (escalate IPC rename)
    work.rs                  ← Phase 5 (retry-work → GuardRequired)
    conflict.rs              ← Phase 5 (combine-conflicting-works → GuardRequired)
    integration.rs           ← Phase 5 (integrate-tick → GuardRequired)
  trigger/observe.rs         ← Phase 1 (has-accepted-bundles state query),
                               Phase 5 (two new guards registered)
  trigger/schema.rs          ← Phase 6 (VALID_EVENTS normalization + lint)
resources/
  engine/triggers/
    agent-lifecycle.yml      ← Phase 2 (reviewer triggers get scope + new event name)
    composites.yml           ← Phase 1 (plan-decomposable composite extended)
    engine.yml               ← Phase 1 (bundles-ready-for-integration fixed)
    reconciliation.yml       ← Phase 1 (plan-active-no-works mirror)
  engine/strategies/
    agent-lifecycle.yml      ← Phase 2 (handle-rejected-bundle split)
    conflict.yml             ← Phase 5 (guard clause added)
    integration.yml          ← Phase 5 (guard clause added)
    recovery.yml             ← Phase 5 (guard clause added)
    supervision.yml          ← Phase 4 (strategy kept; check-threshold works now)
```

### Data Model

No persisted schema changes. The new `AgentEvent::ReviewerVerdict`
variant (Phase 2) is a wire-format addition with no storage side.
JSONL record schemas unchanged. No field added to `AgentSession`
for Bug B — the aggregate query counts existing
`status = Failed` Director sessions.

### API Design

**Bug A — `plan-decomposable` composite fix.**

The `has-no-children` state-query already exists and takes a single
`child-collection`. Compose at the YAML layer rather than implementing
a new multi-collection query.

Add a mirror of `plan-active-no-specs` for works:

```yaml
# resources/engine/triggers/reconciliation.yml
plan-active-no-works:
  type: state-query
  scope: plan
  query: has-no-children
  params:
    child-collection: work
```

Extend the composite to require zero children in either collection:

```yaml
# resources/engine/triggers/composites.yml
plan-decomposable:
  type: composite
  operator: and
  triggers:
    - plan-is-active
    - plan-active-no-specs
    - plan-active-no-works
```

Under brief mode the plan has no specs ever; after the first
decomposer completes the plan has at least one work, so
`plan-active-no-works` becomes false and the composite stops firing.
Under tiered mode (hypothetical) the plan has specs after the first
decomposer completes, so `plan-active-no-specs` becomes false and
the composite stops firing. Either path terminates.

No new state-query implementation required. `has-no-children-any-of`
was overengineering in the Pass 1 draft.

The parallel fix for `spec-decomposable`: today specs never exist in
brief mode so the trigger never fires. We leave it alone. A future
tiered-mode run will re-surface the question.

**Bug B — `restart-director-on-state` repair options.**

Three paths; choose one:

*Option 1: Remove the strategy.* `restart-director-on-event` already
catches director failures via the `director-failed` event trigger.
The level-triggered fallback was a defense against lost events. If
we trust the event path (agent status events are now correctly
kebab-cased and wired into the trigger engine, per Finding 1), the
fallback is redundant. Simplest fix.

*Option 2: Reshape the strategy to use supported primitives.* Keep
the level-trigger intent but compute restart-count from a supported
source. Since `check-threshold` only knows `work` collection,
aggregate restart counts on the plan-level Director session record
would require new primitive support. The on-failure `escalate` call
to `coordinator.escalate` IPC also needs fixing (rename to
`director.escalate` or remove the escalation entirely).

*Option 3: Add support and keep behavior identical.* Extend
`check-threshold`'s `read_numeric_field` to handle `collection:
session` with `field: restart-count`, and rename the IPC method in
the `escalate` primitive to whatever handler survives.

We pick **Option 1** (remove). Justification: Finding 1 proved events
now work; the level-triggered fallback was added back when the event
path was silently broken; with event wiring fixed the redundancy is
unjustified. If future log analysis shows missed director restarts,
we restore the strategy properly with Option 2 at that time.

**Finding 2 — `GuardRequired` restoration with new guards.**

Register two new guards in
`src/trigger/observe.rs::GuardConditionRegistry::with_builtins`:

```rust
// work-not-superseded: work exists and is not in Superseded state.
r.register(
    "work-not-superseded",
    Box::new(|ctx, _collection, id| {
        ctx.stores.works.read().ok()
            .and_then(|works| works.get(id).map(|w| w.status()))
            .map(|s| !matches!(s, WorkStatus::Superseded))
            .unwrap_or(false)
    }),
);

// plan-has-no-active-integration: no in-flight Tick for plan.
// Non-terminal TickStatus variants are Open, Sealing, Validating.
// A guard that blocks while any such tick exists prevents re-entering
// integrate-tick for the same plan while the previous integration is
// still sealing or validating.
r.register(
    "plan-has-no-active-integration",
    Box::new(|ctx, _collection, id| {
        ctx.stores.ticks.read().ok()
            .map(|ticks| !ticks.values().any(|t| {
                t.plan_id == id
                    && matches!(
                        t.status(),
                        TickStatus::Open | TickStatus::Sealing | TickStatus::Validating
                    )
            }))
            .unwrap_or(false)
    }),
);
```

(`no-active-implementer-for-work` already exists; reused as-is.)

For each primitive:
- `retry-work` → `GuardRequired`; `work-retry-on-failure.on-success[0]`
  gets `guard: no-active-implementer-for-work`.
- `combine-conflicting-works` → `GuardRequired`;
  `resolve-structural-conflict.action[0]` gets `guard:
  work-not-superseded`.
- `integrate-tick` → `GuardRequired`;
  `integrate-accepted-bundles.action[0]` gets `guard:
  plan-has-no-active-integration`.

The `handle-rejected-bundle` strategy is entangled with Bug C
(reviewer-rejected trigger has no scope; event payload missing
bundle-id/work-id). Landing a guard on a strategy that cannot fire
with useful scope_ids is premature. We fix Bug C first, which
restructures the reviewer-verdict → retry flow; the guard for
retry-work-on-bundle-rejection is added there, not here.

**Finding 1 residuals — normalize remaining `DaemonEvent` data keys.**

Audit `src/ipc/protocol.rs` for snake_case keys in data payloads.
Convert to kebab-case. Update `director.rs` and any other consumers
that read these keys. Known sites:

| Event | Snake keys | Kebab replacement |
|-------|-----------|-------------------|
| `bundle.merged` | `merged_bundle_ids` | `merged-bundle-ids` |
| `bundle.rejected_stale` | `bundle_work_id`, `latest_tick_id` | `bundle-work-id`, `latest-tick-id` |
| `learning.policy_contradicted` | `learning_id` | `learning-id` |
| `director.stall_detected` | `idle_secs` | `idle-secs` |
| `reconciliation.failed` (severity) | (none) | (verify) |

Bonus: same event names themselves still mix snake and kebab:
`bundle.rejected_stale`, `learning.policy_contradicted`,
`director.mode_changed`, `director.stall_detected`,
`tick.validation_failed`. Rename these to hyphenated forms and update
consumers.

**Finding 3 — typed-struct handler sweep.**

For each handler in `src/daemon/handlers/`, define a local params
struct per IPC method. Pattern:

```rust
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct PlanCreateParams {
    title: String,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

pub(super) fn handle_plan_create(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        let parsed: PlanCreateParams = match serde_json::from_value(req.params.clone()) {
            Ok(p) => p,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params(&e.to_string()))),
        };
        // …use parsed.title, parsed.tier, etc.
    })
}
```

Exclusions (dynamic-payload handlers): `record.create`,
`record.update` — these are generic record mutation APIs that accept
arbitrary collection-specific payloads. Keep them dynamic; do NOT
apply `deny_unknown_fields`.

### Implementation Plan

#### Phase 1: Bugs A + D — decomposer loop + ghost integration trigger
**Model:** opus

Two composite-trigger fixes bundled because both rely on adding or
extending state-query triggers in the same YAML surface.

**Part 1 (Bug A) — decomposer loop.**

- Add `plan-active-no-works` state-query trigger in
  `resources/engine/triggers/reconciliation.yml` as a mirror of
  `plan-active-no-specs` using the existing `has-no-children` query
  with `child-collection: work`.
- Extend `plan-decomposable` composite in
  `resources/engine/triggers/composites.yml` to AND three triggers
  (`plan-is-active`, `plan-active-no-specs`, `plan-active-no-works`).

**Part 2 (Bug D) — ghost integration trigger.**

- Implement a new state-query function `has-accepted-bundles` in
  `src/trigger/observe.rs::StateQueryRegistry::with_builtins`. It
  takes the plan scope_id and counts Bundles where `plan_id ==
  scope_id AND status == Accepted`. Returns true when count > 0.
- Rewrite `bundles-ready-for-integration` in
  `resources/engine/triggers/engine.yml` to use the new
  `has-accepted-bundles` state query instead of `field-equals`:

  ```yaml
  bundles-ready-for-integration:
    type: state-query
    scope: plan
    query: has-accepted-bundles
  ```

- Remove the placeholder comment.

**Tests (src/trigger/tests.rs):**
- Composite `plan-decomposable` fires when plan has zero children;
  does not fire when plan has any Work or Spec child.
- `has-accepted-bundles` query returns true when at least one
  Accepted bundle exists for the plan; false otherwise (no bundles,
  or bundles in other statuses).

Stop criterion: unit tests green; `otto ci` green.

#### Phase 2: Bug C — reviewer verdict → retry pipeline repair
**Model:** opus

The reviewer session currently emits a generic `agent.status-changed`
event with only `session-id` and `status`. This phase introduces a
dedicated verdict event carrying bundle and work context, re-scopes
the triggers that consume it, and splits the retry flow across
strategies whose scopes align with their targets.

**Step 1 — new event variant.** Add to `src/agents/event.rs`:

```rust
ReviewerVerdict {
    session_id: String,
    bundle_id: String,
    work_id: String,
    verdict: ReviewerVerdict,  // approve | reject | request-changes
},
```

Register the event name `"agent.reviewer-verdict"` in
`DaemonEvent` constructors (mirror of existing `agent_status_changed`).
Add the name to `VALID_EVENTS` in `src/trigger/schema.rs`.

**Step 2 — emit the enriched event.** Wherever the reviewer
session's verdict becomes final (search for reviewer-specific code
in `src/agents/executor/lifecycle.rs` and the reviewer agent
entry-point), emit the new event in addition to the generic
`agent.status-changed`. Keep `status-changed` emission for
compatibility with generic lifecycle listeners (TUI, Director
failure detection).

**Step 3 — update triggers.** Rewrite
`resources/engine/triggers/agent-lifecycle.yml`:

```yaml
reviewer-approved:
  type: event
  event: agent.reviewer-verdict
  scope: bundle
  match:
    verdict: approve

reviewer-rejected:
  type: event
  event: agent.reviewer-verdict
  scope: bundle
  match:
    verdict: reject

reviewer-rejected-work:
  type: event
  event: agent.reviewer-verdict
  scope: work
  match:
    verdict: reject
```

Same event, three scope-extraction lenses. `eval_event` supports
multiple triggers matching a single event; each produces its own
`scope_ids` based on its `scope:` field (`bundle-id` or `work-id`
from the payload).

**Step 4 — split the strategy.** Replace `handle-rejected-bundle` in
`resources/engine/strategies/agent-lifecycle.yml` with two
strategies whose scopes align with their mutation targets:

```yaml
reject-bundle-on-reviewer-rejection:
  description: Mark the bundle as Rejected when the reviewer rejects it
  trigger: reviewer-rejected
  scope: bundle
  priority: 820
  action:
    - primitive: transition-record
      params:
        collection: bundle
        id: $trigger.scope-id
        target-status: rejected
        role: reviewer

retry-work-on-bundle-rejection:
  description: Retry the work when its bundle is rejected
  trigger: reviewer-rejected-work
  scope: work
  priority: 810
  action:
    - primitive: retry-work
      # guard clause added in Phase 5
      params:
        work-id: $trigger.scope-id
```

`accept-approved-bundle` stays, but its trigger ref becomes the new
scoped `reviewer-approved`.

**Step 5 — delete** the old `handle-rejected-bundle` strategy.

**Tests:**
- Unit: new `AgentEvent::ReviewerVerdict` round-trips via serde with
  kebab-case keys.
- Integration: emit a reviewer-verdict event with verdict=reject,
  observe `reject-bundle-on-reviewer-rejection` fires exactly once
  (transitions bundle to Rejected) and
  `retry-work-on-bundle-rejection` fires exactly once (retries the
  work).
- Integration: observe throttling — a second emission within
  `throttle-secs` does not re-invoke either strategy.

Stop criterion: reviewer-verdict integration test passes;
`otto ci` green.

#### Phase 3: Bug E — escalate primitive IPC repair
**Model:** opus

The `escalate` primitive calls a removed IPC endpoint. This phase
restores the escalation path that 4 strategies depend on.

**Step 1 — implement the handler.** Create `handle_director_escalate`
in `src/daemon/handlers/director.rs`. Signature:

```rust
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct DirectorEscalateParams {
    reason: String,
    #[serde(default)]
    scope_id: Option<String>,
    // Additional diagnostic fields forwarded from the primitive
    // call site; captured as a catch-all context map only when
    // deny_unknown_fields can't be applied (decision: keep strict
    // for now, revisit if real diagnostic payloads need it).
}
```

The handler's job is to notify the Director to enter its Escalation
mode. Options for the notification mechanism — confirm during
implementation review:

- **Option 1:** set `DirectorMode::Escalation` on the active
  Director session for the plan (if one exists), persist, emit
  `director.mode-changed` event.
- **Option 2:** spawn a new Director session if none active, set
  mode to Escalation at creation.

Either way, the handler MUST be idempotent — multiple escalate
calls for the same plan within a short window should coalesce.

**Step 2 — rename the IPC call.** Update
`src/primitive/catalog/sweep.rs:278` from
`ctx.bridge.request("coordinator.escalate", params)` to
`ctx.bridge.request("director.escalate", params)`.

**Step 3 — register the handler in dispatch.** Wire
`"director.escalate"` in the daemon's request dispatcher to the new
handler.

**Tests:**
- Unit: handler validates params and returns a structured response.
- Integration: emit an `escalate` primitive call; observe the
  Director session transitions to Escalation mode.
- Regression: the `abandon-ratio-escalation` strategy
  (recovery.yml:45) fires end-to-end when its ratio trigger
  matches, and the escalate call does not bail.

Stop criterion: integration test passes; `otto ci` green; grep
finds no remaining references to `coordinator.escalate`.

#### Phase 4: Bug B — check-threshold session support (keep restart-director-on-state)
**Model:** opus

Vision Principle 8 requires the level-triggered fallback. Rather
than delete `restart-director-on-state`, extend `check-threshold` to
understand the `session` collection with aggregate-failed-session
semantics matching the v3 intent ("don't loop-restart Director more
than N times per plan").

**Step 1 — extend check-threshold.** In
`src/primitive/catalog/reconcile.rs`, update
`read_numeric_field` to accept `collection: "session"`:

```rust
"session" => {
    // Parameters: id = role name (literal, e.g., "director")
    // field = "restart-count" (aggregate: count of failed sessions
    //         of this role attached to the scope_id)
    // scope_id comes from the strategy's scope binding
    // (restart-director-on-state runs scope: plan, scope_id: plan_id)
    match (id, field) {
        ("director", "restart-count") => {
            let sessions = ctx.stores.agent_sessions.read()?;
            let count = sessions.values()
                .filter(|s| {
                    s.agent_type == AgentKind::Director
                        && matches!(s.status(), AgentStatus::Failed)
                        // filter by plan scope - requires plumbing
                        // scope_id down into this function, see note
                })
                .count() as u32;
            Ok(count)
        }
        _ => eyre::bail!("unknown session query: id={id} field={field}"),
    }
}
```

**Note on scope plumbing:** `read_numeric_field` currently takes
`(ctx, collection, id, field)`. The primitive resolves `id` and
`field` from the strategy params but does not receive the
strategy's scope_id. For `collection: session`, the aggregate query
needs the plan scope_id to filter sessions. Two options:

- **Option A:** extend `check-threshold` to pass a 5th argument
  `strategy_scope_id` (from the primitive's context). This requires
  threading scope_id into `PrimitiveContext`.
- **Option B:** filter-by-session-role-only (no plan scoping); relies
  on the fact that only one plan is active in v4's current single-plan
  invariant.

**Pick Option A** for correctness even though v4 is single-plan
today; the scope-aware version future-proofs multi-plan.

**Step 2 — verify the strategy works.** With `check-threshold`
supporting session, `restart-director-on-state` now evaluates
correctly. The `escalate` call in its `on-failure` block now works
because Phase 3 landed.

**Tests:**
- Unit: `read_numeric_field("session", "director", "restart-count")`
  returns the correct count for a set of mock sessions.
- Integration: simulate 6 failed Director sessions on a plan;
  `check-threshold` reports exceeded; `escalate` fires instead of
  `spawn-agent`.

Stop criterion: integration test passes; the two Round-3 WARNs
are gone from daemon startup.

#### Phase 5: Finding 2 — restore GuardRequired with new guards
**Model:** opus

- Register `work-not-superseded` and `plan-has-no-active-integration`
  guards in `GuardConditionRegistry::with_builtins`. (The third —
  `no-active-implementer-for-work` — already exists.)
- Add `guard:` clauses to each strategy step invoking one of the
  three primitives:
  - `work-retry-on-failure.on-success[0]`:
    `guard: no-active-implementer-for-work`
  - `resolve-structural-conflict.action[0]`:
    `guard: work-not-superseded`
  - `integrate-accepted-bundles.action[0]`:
    `guard: plan-has-no-active-integration`
  - `retry-work-on-bundle-rejection.action[0]`:
    `guard: no-active-implementer-for-work` (added in Phase 2)
- Flip the three primitives' `Idempotency` from `NonIdempotent` back
  to `GuardRequired`.
- Regression test `engine::tests::strategy_catalog_passes_primitive_aware_gates`
  must still pass (now exercising the guard clauses).
- Add one new test per newly-registered guard exercising both firing
  and blocking paths.

Stop criterion: all primitive-aware tests pass; `otto ci` green.

#### Phase 6: Finding 1 residuals — normalize remaining snake keys
**Model:** sonnet

- Sweep `src/ipc/protocol.rs` for snake keys in `serde_json::json!({…})`
  data payloads inside `DaemonEvent::*` constructors. Convert each to
  kebab-case.
- Rename mixed event names: `bundle.rejected_stale` →
  `bundle.rejected-stale`, `learning.policy_contradicted` →
  `learning.policy-contradicted`, `director.mode_changed` →
  `director.mode-changed`, `director.stall_detected` →
  `director.stall-detected`, `tick.validation_failed` →
  `tick.validation-failed`.
- Audit and update consumers in `src/agents/director.rs`,
  `src/tui/run/events.rs`, `src/tui/run/ipc.rs`,
  `src/trigger/schema.rs` (`VALID_EVENTS`), trigger YAMLs, and tests.
- Add a startup lint or test-only check that fails when the set of
  event names in `schema.rs::VALID_EVENTS` does not match the set of
  event names emitted by `DaemonEvent::*` constructors. This is the
  structural fix that prevents future drift of the kind Finding 1
  addressed.

Stop criterion: grep finds no `"[a-z]+\.[a-z]+_[a-z]+"` event-name
literals emitted from `DaemonEvent::*` constructors anywhere in
`src/**/*.rs`; event-catalog consistency test green; `otto ci` green.

#### Phase 7: Finding 3 — typed-struct handler sweep, one commit per handler
**Model:** sonnet

Order by dependency risk (lowest-risk first):

1. `system.rs` — system.status has no params; only handshake needs a
   struct.
2. `tools.rs` (3 dyn calls)
3. `chat.rs` (2 dyn calls — one method already uses a struct, wire the
   remaining two)
4. `doc.rs` (4 dyn calls)
5. `director.rs` (5 dyn calls)
6. `worktree.rs` (6 dyn calls — has 1 struct; 5 more to add)
7. `agent.rs` (8 dyn calls — has 1 struct; 7 more to add)
8. `decomposer.rs` (9 dyn calls)
9. `lock.rs` (10 dyn calls)
10. `plan.rs` (10 dyn calls)
11. `integrator.rs` (11 dyn calls)
12. `tick.rs` (11 dyn calls)
13. `phase.rs` (13 dyn calls)
14. `spec.rs` (13 dyn calls)
15. `work.rs` (16 dyn calls)
16. `learning.rs` (19 dyn calls — has 1 struct; 18 more to add)
17. `bundle.rs` (21 dyn calls — has 1 struct; 20 more to add)

Each handler file gets its own commit:
`refactor(handlers): typed params for <handler>`. After each commit,
`otto ci`. If any commit reveals a callers-use-wrong-key bug, file
it as a separate finding and fix forward.

Skip `record.create` and `record.update` methods (if they live in any
of these files) per the Architect's exclusion — they are dynamic
payload APIs.

Stop criterion: `grep -r 'req\.params\.get\|req\.params\['
src/daemon/handlers/` returns only tracing instrumentation fields
(`#[instrument(…fields(foo = ?req.params.get("foo")))]`) and explicit
dynamic-payload sites; no production read paths use string-literal
param lookups.

#### Phase 8: E2E validation
**Model:** sonnet

- Run `/e2e rust-version`. Expect exit 0 with target `src/main.rs`
  modified to handle `--version`.
- If timeout or failure: diagnose, file specific ticket, do NOT
  force-merge.
- Run `/e2e python-todo` as a second validation (larger task, fuller
  pipeline exercise).
- Report deltas and ship.

Stop criterion: `rust-version` E2E passes (exit 0, target
implemented).

## Alternatives Considered

### Alternative 1: Narrowest possible fix — only do Bug A and Bug B
- **Description:** Fix only the two blocking E2E bugs, leave Finding
  1 residuals and Finding 3 typed-struct sweep for an indefinite
  future.
- **Pros:** Fastest path to a passing E2E.
- **Cons:** Leaves the Architect's Finding 3 request unresolved; the
  dynamic-key vulnerability stays in place; Finding 1 has lingering
  inconsistencies (snake keys for `learning_id` etc.) that will
  confuse the next contributor and are exactly the class of bug
  Finding 1 was supposed to eliminate.
- **Why not chosen:** We already have the momentum from Round 3 and
  the refactoring is mechanical. Doing it in one round is cheaper
  than re-gathering context later.

### Alternative 2: Keep `NonIdempotent` for the three primitives
- **Description:** Accept the Round 3 `NonIdempotent` choice; skip
  Phase 5 of this plan.
- **Pros:** No new guards to register; no strategy YAML churn; no
  scope-mismatch workaround.
- **Cons:** Per advisor flag, Architect likely to push back in
  Round 4. `NonIdempotent` offers no *enforcement* — "must be last"
  is a documentation convention the gate cannot verify. A contributor
  adding a step after `retry-work` would silently introduce a
  correctness bug.
- **Why not chosen:** The gate should enforce safety, not rely on
  human memory.

### Alternative 3: Extend guard signature to accept step params
- **Description:** Change `GuardConditionFn` to
  `(ctx, collection, id, step_params) -> bool`, allowing guards to
  read parameters from the invoking step (e.g., work-id from a
  bundle-scoped strategy).
- **Pros:** Unlocks every cross-scope guard case without
  strategy-level contortions.
- **Cons:** Invasive — every existing guard gets a new parameter
  even if unused. Changes the semantic contract of the guard
  (guards become strategy-aware, blurring the boundary between
  trigger observation and strategy execution). Broader review
  surface.
- **Why not chosen:** Phase 2's trigger-scoping approach covers all
  three cases we need to fix now by aligning the strategy scope with
  the record the strategy mutates. If a future fix requires
  genuinely cross-scope guards, revisit.

### Alternative 4 (CHOSEN): Fix Bug B by supporting session in check-threshold
- **Description:** Extend `check-threshold::read_numeric_field` to
  handle `session` collection with aggregate-failed-session
  semantics. Rename `coordinator.escalate` IPC to `director.escalate`
  and implement the handler (Phase 3). Restore the original
  level-triggered restart behavior.
- **Pros:** Preserves the level-triggered fallback mandated by
  Vision Principle 8 ("Event triggers need level-triggered
  fallbacks").
- **Cons:** Adds code paths in `check-threshold` and the IPC
  handler registry. Requires scope plumbing (scope_id down into
  `PrimitiveContext`) — see Phase 4 Option A.
- **Why chosen:** Vision Principle 8 explicit. Deleting the strategy
  violates the architectural pattern. The implementation cost is
  small and unlocks the correct behavior the vision requires.

### Alternative 5: Extend StatusChange instead of adding ReviewerVerdict variant
- **Description:** Rather than introducing a new `AgentEvent`
  variant in Phase 2, extend the existing `StatusChange` with
  optional `bundle-id`, `work-id`, and `verdict` fields that only
  the reviewer populates.
- **Pros:** Fewer event-name registrations; consumers that listen
  to status changes keep working unchanged.
- **Cons:** Violates single-responsibility — `StatusChange` now
  carries role-specific context. Trigger matching relies on
  optional-field presence checks, less explicit. The enum's type
  tag says "status-change" but the payload says "verdict".
- **Why not chosen:** A new variant is cleaner for pattern matching
  and for `VALID_EVENTS` discoverability. The cost of the
  new-variant path (one enum line + one constructor + one
  `VALID_EVENTS` entry) is trivial.

## Technical Considerations

### Dependencies

- No new crate dependencies.
- Internal: the new guard registrations and state-query
  implementations reuse existing patterns in
  `src/trigger/observe.rs`.

### Performance

- Adding one state-query evaluation per tick for
  `plan-has-no-descendants` is negligible (reads two in-memory
  HashMaps for child counts).
- Three new guards each add at most one HashMap read per strategy
  fire; total tick cost change is below measurement noise.
- Typed-params parsing vs. `params.get("…")`: serde_json's
  `from_value` is marginally slower than direct key lookup but
  happens once per IPC request; not hot path.

### Security

- `deny_unknown_fields` on handler params is a security *improvement*
  (rejects malformed requests at the boundary).
- No new attack surface introduced.

### Testing Strategy

| Phase | Test Plan |
|-------|-----------|
| 1 | Unit tests for new state query + composite; regression test that `plan-decomposable` fires exactly once for a brief plan |
| 2 | `otto ci`; daemon startup log check |
| 3 | Existing `strategy_catalog_passes_primitive_aware_gates`; new per-guard unit tests |
| 4 | Event-catalog consistency test; grep-based lint |
| 5 | `otto ci` after each handler commit; no new integration tests needed |
| 6 | End-to-end via `/e2e rust-version`; secondary `/e2e python-todo` |

### Rollout Plan

- Each phase lands as its own commit or commit cluster. Version
  bumped once at the end.
- No migration steps — all changes are code or YAML; no persisted
  schema evolution.
- If any phase fails `otto ci`, fix forward within that phase. Never
  leave a partial phase merged.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Bug A fix under-scoped; same loop re-appears in a tiered-mode plan via different composite | Medium | High | Unit test covers both brief and tiered configurations; composite ANDs both `plan-active-no-specs` and `plan-active-no-works`, so either tier terminates |
| Phase 2 (Bug C) reviewer verdict split introduces trigger double-firing — both `reviewer-rejected` (scope: bundle) and `reviewer-rejected-work` (scope: work) match the same event | Medium | Low | Intended: each trigger binds its own strategy at its own scope; evaluator already supports multiple triggers matching the same event. Test asserts each strategy fires exactly once per verdict event |
| Phase 2 enriched event breaks existing reviewer event consumers (TUI, chat funnel) | Medium | Medium | Audit all consumers of `agent.status-changed` for reviewer sessions during Phase 2; update them to consume `agent.reviewer-verdict` for verdict-specific logic and keep using `agent.status-changed` for generic status transitions. Both events fire for reviewer terminals |
| Phase 5 guard `plan-has-no-active-integration` blocks forever on a stale Open/Sealing/Validating Tick that never reaches terminal | Low | High | Orthogonal staleness concern. If observed, the existing reconciliation sweep should catch stuck ticks; note as Round 5 follow-up if it actually occurs |
| Phase 6 event rename breaks undiscovered consumers (third-party tools reading the daemon event stream) | Low | Medium | Locked: hard rename, no `serde(alias)` window per v4-vision clean-break policy. Pre-release software has no third-party consumers yet |
| Phase 6 `VALID_EVENTS` catalog consistency check flags existing unreferenced events (e.g. `agent.created` that is listed but never emitted) | High | Low | Expected: deal with them in Phase 6 — either emit them if they should be, or remove from the catalog. Enumerate the gap before writing the lint so the lint's first run passes |
| Phase 7 typed-struct sweep uncovers latent caller-side bugs (callers send wrong kebab keys) | Medium | Low | Each commit runs `otto ci`; any caller drift reveals itself as a parse error and gets fixed in the same commit. Exclude `record.create`/`record.update` from `deny_unknown_fields` per Architect directive |
| Phase 3 director.escalate handler is idempotent but an escalation storm (many escalate calls for the same plan in seconds) still spawns excessive work | Low | Medium | Handler debounces by checking existing Director session mode before acting; coalesces within a short cooldown window (seconds) |
| E2E still fails after Phases 1-5 because of a third pipeline bug not yet identified | Medium | Medium | After Phase 5 lands, run an interim E2E dry run. If it still fails with a new bug, file it and decide whether to fold in or spin out before proceeding |
| Reviewer session is attached to multiple bundles simultaneously (unlikely but possible in future) | Low | Low | Current `AgentSession.bundle_id: Option<String>` is one-to-one; if this invariant changes, the event-enrichment code needs revisiting. Note in commit body |

## Open Questions

- [ ] Phase 2: should the enriched reviewer event be a new variant
  `AgentEvent::ReviewerVerdict { session-id, bundle-id, work-id,
  verdict }`, or should we extend `StatusChange` with optional
  context fields? The new-variant approach is cleaner for pattern
  matching; the field-extension approach avoids a new event name.
  Leaning toward new variant; confirm during implementation review.

- [ ] Phase 3: which of the two director-notification options does
  the handler implement — set mode on existing Director session, or
  spawn-if-absent? Confirm during implementation when the Director
  escalation flow is inspected in `src/agents/director.rs`.

- [ ] Phase 7: encounters `ValidateAndPublishTickParams` — the
  integrator handler is one of the largest. Is there value in
  decomposing the handler file into per-method sub-modules while
  we're editing it, or is that scope creep for this round?
  Default: scope creep; do it in a separate design doc if justified.

- [ ] Phase 7: should we also kebab-case the `FunnelState` variants
  in `src/domain/chat.rs`? Currently `rename_all = "snake_case"` for
  persistence compatibility. Not in scope unless we're changing
  persisted formats (we're not).

- [ ] Interim E2E run after Phase 5: if it reveals a fourth pipeline
  bug, do we fold it in or spin it out? Default: fold in if the fix
  is <200 lines and reaches consensus during review; otherwise spin
  out to preserve scope discipline.

## References

- `docs/design/2026-04-17-strategy-primitive-wiring.md` — the Round 3
  design and its appended Round 3 Findings section.
- E2E run: `/tmp/loopr/e2e/rust-version/20260417-152358/` —
  session log showing the `plan-decomposable` loop and the
  `restart-director-on-state` WARNs.
- `resources/engine/triggers/composites.yml:25` — `plan-decomposable`
  composite.
- `resources/engine/strategies/supervision.yml:32` —
  `restart-director-on-state` strategy.
- `src/primitive/catalog/reconcile.rs:310` — `check-threshold`
  collection gate.
- `src/primitive/catalog/sweep.rs:278` — `escalate` primitive with
  dead `coordinator.escalate` call.
- `src/trigger/observe.rs:432-620` — existing guard registry and
  patterns for adding new guards.
- `src/daemon/handlers/agent.rs:17-34` — `AgentStartParams` as the
  reference pattern for Finding 3 typed-struct work.
- `resources/engine/triggers/agent-lifecycle.yml:29-45` —
  `reviewer-approved` and `reviewer-rejected` trigger definitions
  missing `scope:`.
- `resources/engine/strategies/agent-lifecycle.yml` —
  `handle-rejected-bundle` strategy that cannot resolve its targets.
- `src/ipc/protocol.rs:260-328` — `DaemonEvent::agent_*` constructors
  showing both the correctly-kebabed agent events from Round 3 and
  the residual snake keys in other constructors (`learning_id`,
  `idle_secs`, `merged_bundle_ids`, `bundle_work_id`,
  `latest_tick_id`).
- `src/agents/event.rs:6-48` — `AgentEvent` enum where the new
  `ReviewerVerdict` variant lands in Phase 2.
