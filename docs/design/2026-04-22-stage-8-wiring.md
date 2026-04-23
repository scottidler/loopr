# Design Document: Stage 8 Wiring - Plan to Tick

**Author:** Claude (with Scott)
**Date:** 2026-04-22
**Status:** Implemented
**Review Passes Completed:** 5/5 (self-review) + Architect R1 folded (2026-04-22)
**Shipped in:** v0.5.29
**Crates touched:** `loopr` (primary), `domain` (one FSM-edge addition: `WorkStatus: InReview => Blocked by Coordinator` in the overrides table), `store` (one new accessor: `WorksStore::list_by_parent_id`), `integrator` (consumer), `agents` (consumer).

## Summary

Wire the daemon so an approved Bundle actually reaches the integration branch and produces a Tick. Stage 8 shipped the Reviewer and Integrator as per-crate designs; neither is reachable from `handle_plan_create`. This doc owns the cross-crate glue: integration-branch creation at Plan time, the Work-FSM orchestration the pipeline has been silently missing, Reviewer and Integrator `spawn_*_for_*` methods parallel to `spawn_implementer_for_work`, a circuit-broken retry policy for `Integrating` Bundles, and a restart-reconcile sweep that re-enqueues Bundles stranded at every intermediate state.

## Problem Statement

### Background

Stage 8's three component docs all landed Implemented:

- `docs/design/2026-04-22-reviewer.md` - `run_reviewer(&Bundle, &Work, &ReviewerDeps) -> Result<Verdict, ReviewerError>` (v0.5.26).
- `docs/design/2026-04-22-integrator.md` - `integrate(&[Bundle], &Plan, &IntegratorDeps) -> Result<Tick, IntegrationError>` (v0.5.28).
- `docs/design/2026-04-22-stage-7-wiring.md` - first Stage-wiring capstone; established the pattern this doc extends.

Stage 7 wiring ends at a persisted `Bundle` at `BundleStatus::Proposed` with a commit on the worktree branch. Nothing downstream runs. `handle_plan_create` persists the Plan, decomposes, persists Works (born `WorkStatus::Pending`), spawns Implementer tasks, and returns. The Implementer produces a Bundle; the task logs it and exits. No triage, no Reviewer, no Integrator, no Tick.

### Problem

Four concrete gaps block the Stage 8 exit criterion ("approved Bundle lands on the integration branch and produces a Tick record"):

1. **No integration branch.** `loopr/plan-<plan-id>` is never created. `integrate` returns `IntegrationBranchMissing` for every Plan.
2. **No Reviewer dispatch.** Bundles stay at `Proposed`. `run_reviewer` is unreachable.
3. **No Integrator dispatch.** Even if Bundles reached `Accepted`, nothing calls `integrate`.
4. **Work-FSM is silently broken.** `Work::new` returns `Pending`. Nothing transitions Pending -> Ready -> InProgress -> InReview -> Integrated -> Done. `spawn_implementer_for_work::mark_blocked` mutates `work.status = WorkStatus::Blocked` by raw assignment, bypassing `Work::transition`; `Pending -> Blocked` isn't even an FSM edge. First-gate E2E can't read a coherent Work status at any point.

Stage 8's exit criterion is a cross-crate E2E behavior; the per-crate docs legitimately scoped out the dispatch. This doc is where it lands.

### Goals

- `loopr/plan-<plan-id>` branch created at `handle_plan_create` time from the target's current HEAD, before any Implementer is spawned.
- `DaemonContext::spawn_reviewer_for_bundle(self: Arc<Self>, Bundle)` method: triages `Proposed -> Triaged` via `Role::Coordinator`, constructs `ReviewerDeps`, runs `run_reviewer`, routes the `Verdict` (`Accept` -> `Reviewed -> Accepted` + spawn Integrator; `ChangeRequested`/`Reject` -> Work transition to `Blocked`).
- `DaemonContext::spawn_integrator_for_bundle(self: Arc<Self>, Bundle)` method: assembles `IntegratorDeps`, calls `integrate(&[bundle], &plan, &deps)`, transitions each merged Bundle's Work `InReview -> Integrated -> Done` on success, retries with exponential backoff on transient errors, marks `Failed/Blocked` on circuit-breaker exhaustion or terminal `IntegrationError`.
- Work-FSM orchestration across every stage boundary, using `Role::Implementer` / `Role::Integrator` / `Role::Coordinator` as identifiers when the daemon fires transitions on behalf of an actor. No more raw `work.status =` mutations.
- `DaemonContext` grows three fields: `reviewer_config: ReviewerConfig`, `integrator_config: IntegratorConfig`, `git_lock: Arc<Mutex<()>>`; and two JoinSets: `reviewer_tasks`, `integrator_tasks`. Shutdown drains them in order before `Arc::try_unwrap`.
- Extended `daemon::startup::reconcile`: beyond the existing worktree hygiene pass, sweep the Bundle collection for every intermediate status (`Proposed`, `Triaged`, `Reviewed`, `Accepted`, `Integrating`) and re-enqueue the correct next pipeline stage.
- Integration test in `crates/loopr/tests/stage_8_plan_to_tick.rs`: stubbed Implementer and Reviewer LLMs, real git, asserts a row in `.loopr/taskstore/ticks.jsonl` and `Bundle.status == Merged` and `Work.status == Done`.
- Stage 8 exit criterion met: `handle_plan_create` on a toy target produces a `Tick` record where `integration_sha` points at a merge commit on `loopr/plan-<plan-id>` that contains the Implementer's file edit.

### Non-Goals

Structural (deferred to other docs):

- **Stage 9 live-LLM E2E.** Stage 8 asserts against the pipeline with stubbed LLMs; Stage 9's own design doc wires the `scaffold-rust-repo`-generated `rust-version` target with a real Anthropic client. The smoke test named in Phase 5 below is the stubbed-LLM version.
- **Director escalation.** `ChangeRequested` and `Reject` verdicts mark the Work `Blocked`; the capstone does not spawn a re-Implementer attempt or a Director. Vision line 607: "No Director unless escalation triggers... escalation turns into exit-with-error." Retry-on-reject is earned when a real run surfaces a recoverable reject pattern.
- **Researcher agent.** Vision line 597. Not referenced anywhere in this wiring.
- **Multi-Bundle Ticks.** `IntegratorConfig::allow_multi_bundle` stays `false`. Per-Bundle Integrator dispatch only.
- **Parallel worktrees / Works.** Vision line 609: "one Work at a time until serial proves the shape." Serial per-Plan is the Stage 8 scope. (Serial happens naturally because `integrate` holds `git_lock` for its full duration; Implementer/Reviewer LLM calls run concurrently across Works but none touch the working tree.)
- **Budget enforcement.** Vision's per-Work / per-run soft-pause is not wired. `DaemonContext::can_spawn_new_work()` stays a `true`-returning placeholder from Stage 7.
- **Work-only crash recovery.** `sweep_bundles` recovers any Bundle in an intermediate FSM state, but a Work crashed at `InProgress` *before* producing a Bundle (Implementer crashed mid-run) is not re-enqueued. Stage 7's reconcile doc already noted this depends on `Work.failure_reason` + a `mark_crash_interrupted` mutator that do not yet exist in `domain` / `store`. First-gate smoke test (Phase 4) runs within a single daemon lifetime, so this gap is not blocking. A follow-up doc lands the Work-level sweep when a real run surfaces the stranded-Work case.
- **Event bus.** Dispatch stays inline (Stage 7 precedent + vision's "typed event bus" in Deferred Enhancements). When polling/inline handoff becomes the bottleneck, a design doc earns the event bus.
- **Capstone-pattern-as-roadmap-policy.** The Integrator doc and Stage-7 wiring doc both raised the observation that every multi-crate stage needs a capstone. A separate doc (or an amendment to `docs/roadmap.md`) promotes it to policy; this doc just practices the pattern.

Feature scope (deferred until a real run asks):

- **Reviewer retry on parse failures / escalation.** `run_reviewer` already handles parse-retry internally (`ReviewerConfig::max_requeries`). Daemon-side retry wraps that with "on `ReviewerError::EscalationNeeded`, mark Work Blocked." No further wrapping.
- **Implementer retry on Bundle reject.** First gate is pass-or-block. A future design doc adds bounded attempt counters and a fresh worktree per attempt; the `Worktree::create` API already supports `<work-id>-<seq>` from Stage 7.

## Proposed Solution

### Overview

One paragraph. `handle_plan_create` persists Plan + Works, creates `loopr/plan-<plan-id>` at target HEAD, and spawns one Implementer task per Work into `ctx.implementer_tasks` (Stage 7 shape unchanged except for the new branch creation). The Implementer task, on success, transitions its Work `InProgress -> InReview` via `Role::Implementer` identifier, then spawns a Reviewer task into `ctx.reviewer_tasks` before exiting. The Reviewer task triages the Bundle `Proposed -> Triaged` via `Role::Coordinator`, runs `run_reviewer`, and on `Verdict::Accept` transitions the Bundle `Reviewed -> Accepted` via `Role::Coordinator` and spawns an Integrator task into `ctx.integrator_tasks`; on `ChangeRequested` / `Reject` it transitions the Work to `Blocked` and exits (no re-implementer for first gate). The Integrator task calls `integrate(&[bundle], &plan, &deps)` with a 5-attempt circuit breaker on `Stale` / transient `Store(...)`; on `Ok(tick)` it transitions each merged Bundle's Work `InReview -> Integrated -> Done`; on terminal `IntegrationError` variants it transitions the Work to `Blocked`. Daemon startup's `reconcile` sweep extends from worktree-hygiene to Bundle-FSM sweep, so a crash at any pipeline stage is recovered by re-enqueueing the right `spawn_*_for_*` task for each stranded Bundle before the IPC listener binds.

### Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│ handle_plan_create(ctx, params)                                         │
│  1. plan = Plan::new(goal)    // born PlanStatus::Active (per plan.rs)  │
│     // PlanId exists BEFORE any store write, used for branch creation   │
│  2. ensure_integration_branch(&ctx.target, &plan.id)  // NEW            │
│       on Err -> return RpcError::Internal                               │
│                 (NO store writes yet; no orphan records left behind)    │
│  3. ctx.store.plans().create(plan)                                      │
│  4. decomposer::decompose(plan, target, llm) -> Vec<Work> (all Pending) │
│  5. ctx.store.works().create_many(works)                                │
│  6. for work in works:                                                  │
│        Coordinator: Pending -> Ready -> InProgress  (two transitions)   │
│        ctx.implementer_tasks.spawn(spawn_implementer_for_work(work))    │
│                                                                         │
│  Plan stays Active until the Integrator drives it to Complete (see      │
│  spawn_integrator_for_bundle below). No Draft->Active step: Plan::new   │
│  births Active per first-gate decision in plan.rs.                      │
└───────────────────────────────┬─────────────────────────────────────────┘
                                │
                                ▼
┌─────────────────────────────────────────────────────────────────────────┐
│ spawn_implementer_for_work(self: Arc<Self>, mut work)  (AMENDED)        │
│  ... existing body ...                                                  │
│  Ok(bundle) =>                                                          │
│    Implementer: work InProgress -> InReview   (role identifier)         │
│    ctx.reviewer_tasks.spawn(                                            │
│      Arc::clone(self).spawn_reviewer_for_bundle(bundle))                │
│  Err(EscalationNeeded) | Err(_) =>                                      │
│    Coordinator: work InProgress -> Blocked                              │
│     (replaces the raw `work.status = Blocked` at context.rs:321)        │
└───────────────────────────────┬─────────────────────────────────────────┘
                                │
                                ▼
┌─────────────────────────────────────────────────────────────────────────┐
│ spawn_reviewer_for_bundle(self: Arc<Self>, bundle)  (NEW)               │
│  1. Coordinator: bundle Proposed -> Triaged                             │
│  2. work = ctx.store.works().get(&bundle.work_id).await                 │
│       (on Err: log, return - task exits, Work unchanged on disk)        │
│  3. Work state repair BEFORE run_reviewer (not after):                  │
│       if work.status == InProgress:   // reconcile-recovery path        │
│         Coordinator: work InProgress -> InReview (override)             │
│       // Rationale: a Proposed Bundle is proof the Implementer finished │
│       // persisting; if the Work is still at InProgress, the Implementer│
│       // crashed between Bundle-persist and its own Work transition.    │
│       // Repair BEFORE the 30+s LLM call so the store is consistent for │
│       // the entire review window.                                      │
│  4. deps = ReviewerDeps {                                               │
│       llm: Arc::clone(&ctx.llm),                                        │
│       store: &ctx.store,   // BundleUpdateSink impl                     │
│       context: Arc::clone(&ctx.context_builder),                        │
│       config: ctx.reviewer_config.clone(),                              │
│       target: ctx.target.clone(),                                       │
│     }                                                                   │
│  5. verdict = run_reviewer(&bundle_as_triaged, &work, &deps).await      │
│       (on Err: Coordinator work InReview -> Blocked (override); return) │
│  6. match verdict {                                                     │
│       Accept =>                                                         │
│         Coordinator: bundle Reviewed -> Accepted                        │
│         ctx.integrator_tasks.spawn(                                     │
│           Arc::clone(self).spawn_integrator_for_bundle(bundle))         │
│       ChangeRequested | Reject =>                                       │
│         Coordinator: work InReview -> Blocked (override)                │
│         // one-step using the new edge added in Phase 1 to domain.      │
│         // no re-implementer, no Director. first-gate scope.            │
│     }                                                                   │
└───────────────────────────────┬─────────────────────────────────────────┘
                                │
                                ▼
┌─────────────────────────────────────────────────────────────────────────┐
│ spawn_integrator_for_bundle(self: Arc<Self>, bundle)  (NEW)             │
│  1. work = ctx.store.works().get(&bundle.work_id).await                 │
│     plan = ctx.store.plans().get(&work.parent_id).await                 │
│       (on Err of either: log, return - no retry)                        │
│  2. deps  = IntegratorDeps {                                            │
│       bundle_sink: &ctx.store,                                          │
│       works:       &ctx.store,                                          │
│       ticks:       &ctx.store,                                          │
│       config:      ctx.integrator_config.clone(),                       │
│       target:      ctx.target.clone(),                                  │
│       git_lock:    Arc::clone(&ctx.git_lock),                           │
│     }                                                                   │
│  3. retry loop with backoff schedule [100ms, 500ms, 2s, 5s, 5s]:        │
│        match integrate(&[bundle.clone()], &plan, &deps).await {         │
│          Ok(tick) => break Ok(tick)                                     │
│          Err(Update(Stale)) | Err(Store(_)) => backoff, retry           │
│          Err(terminal)                    => break Err(terminal)        │
│        }                                                                │
│     // 5 attempts exhausted -> circuit break: mark Work Blocked          │
│  4. on Ok(tick):                                                        │
│       Integrator: work InReview -> Integrated                           │
│       Coordinator: work Integrated -> Done                              │
│       // Plan check: if every Work under plan.id is terminal with       │
│       // at least one Done,                                             │
│       Coordinator: plan Active -> Complete                              │
│  5. on terminal err or circuit-break:                                   │
│       Coordinator: work InReview -> Blocked (override)  // one-step     │
└───────────────────────────────┬─────────────────────────────────────────┘
                                │
                                ▼
                           Tick persisted.
                    `.loopr/taskstore/ticks.jsonl` line.
                     Stage 8 exit criterion.


┌─────────────────────────────────────────────────────────────────────────┐
│ daemon::startup::reconcile(target, store, ctx)  (EXTENDED)              │
│  existing: worktree hygiene sweep (terminal Works -> cleanup)            │
│  NEW: Bundle-FSM sweep                                                  │
│                                                                         │
│  for bundle in store.bundles().list().await?:                           │
│    match bundle.status {                                                │
│      Proposed    => ctx.reviewer_tasks.spawn(                           │
│                       Arc::clone(ctx).spawn_reviewer_for_bundle(b))     │
│      Triaged     => same (reviewer is idempotent-ish on re-triage:      │
│                       its precondition check accepts Triaged)           │
│      Reviewed    => Coordinator: Reviewed -> Accepted;                  │
│                     ctx.integrator_tasks.spawn(                         │
│                       Arc::clone(ctx).spawn_integrator_for_bundle(b))   │
│      Accepted    => ctx.integrator_tasks.spawn(                         │
│                       Arc::clone(ctx).spawn_integrator_for_bundle(b))   │
│      Integrating => same (integrate idempotency via is-ancestor)        │
│      Merged | Rejected | IntegrationFailed | Superseded => noop         │
│    }                                                                    │
│                                                                         │
│  Sweep runs BEFORE accept_loop binds, so handler-spawned tasks          │
│  cannot race with re-enqueueing.                                        │
└─────────────────────────────────────────────────────────────────────────┘
```

### Data Model

**New fields on `crates/loopr/src/daemon/context.rs::DaemonContext`:**

```rust
pub struct DaemonContext {
    // ... all existing fields ...

    /// Reviewer runtime knobs (max_requeries, diff caps). Cloned per
    /// `spawn_reviewer_for_bundle`. Loaded from `.loopr/config.yml`
    /// `agents.reviewer` at startup; default if absent.
    pub reviewer_config: agents::ReviewerConfig,

    /// Integrator runtime knobs (git_timeout, allow_multi_bundle).
    /// Cloned per `spawn_integrator_for_bundle`. Loaded from config;
    /// default if absent.
    pub integrator_config: integrator::IntegratorConfig,

    /// Intra-daemon working-tree serializer. Held by `integrate` for
    /// the full checkout-merge-rollback sequence. Stage 8 scope: one
    /// active integration at a time per daemon. Multi-Plan concurrency
    /// via per-plan worktrees is Alternative 3.
    pub git_lock: Arc<tokio::sync::Mutex<()>>,

    /// In-flight Reviewer tasks. Drained on shutdown before
    /// `Arc::try_unwrap`.
    pub reviewer_tasks: tokio::sync::Mutex<tokio::task::JoinSet<()>>,

    /// In-flight Integrator tasks. Drained on shutdown before
    /// `Arc::try_unwrap`, AFTER `reviewer_tasks` (reviewers can
    /// enqueue new integrators; integrators never enqueue reviewers).
    pub integrator_tasks: tokio::sync::Mutex<tokio::task::JoinSet<()>>,
}
```

Daemon startup grows the three `ReviewerConfig` / `IntegratorConfig` / `git_lock` constructors, wired through `DaemonContext::new`. `Config::load` exposes `config.agents.reviewer` (already defined in `agents::AgentsConfig`) and `config.integrator` (new composition in `crates/loopr/src/config.rs`).

**One FSM-edge addition, otherwise no new records.** Phase 1 adds a single override edge to `domain::WorkStatus`:

```
overrides(
    ...
    InReview   => Blocked by (Coordinator),   // NEW
)
```

The add closes the partial-fail orphan risk the two-step `InReview -> Ready -> Blocked` path would have created if the second step failed (Work would stay at `Ready` and look fresh to the reconcile sweep). One-step edge, one FSM call, one Open Question resolved.

`WorkStatus` edges this doc consumes:

- `Pending -> Ready by Coordinator` (transition)
- `Ready -> InProgress by Coordinator` (transition)
- `InProgress -> InReview by Implementer` (transition) - daemon uses `Role::Implementer` identifier
- `InProgress -> Blocked by Coordinator` (transition) - covers Implementer-err path
- `InProgress -> InReview by Coordinator` (override) - used by reconcile-spawned Reviewer to pull Work up when Implementer crashed before firing its own transition. Fired at triage time, before `run_reviewer`.
- `InReview -> Integrated by Integrator` (transition) - daemon uses `Role::Integrator` identifier
- `Integrated -> Done by Coordinator` (transition)
- **`InReview -> Blocked by Coordinator` (override, NEW)** - used on reject verdict, reviewer error, and integrator terminal error / circuit-break

`PlanStatus`:

- `Active -> Complete by Coordinator` (transition) - fired by the Integrator spawn after its `Integrated -> Done` step, if a Plan-level check confirms every child Work is terminal with at least one `Done`. NO `Draft -> Active` step: `Plan::new` births `Active` per `crates/domain/src/plan.rs:80` ("Later stages may reintroduce Draft as the birth state"); first-gate has no interview loop.

`BundleStatus`:

- `Proposed -> Triaged by Coordinator` (transition)
- `Reviewed -> Accepted by Coordinator` (transition)

Work-state repair on reconcile-recovery: a Reviewer spawned by `sweep_bundles` against a `Proposed` Bundle can meet a Work at `InProgress` (Implementer crashed between Bundle-persist and its own `InProgress -> InReview` transition). The fix lives at the triage step (Step 3 of `spawn_reviewer_for_bundle`), not at the Accept branch: immediately after `Proposed -> Triaged`, check `work.status`; if `InProgress`, fire `Coordinator: InProgress -> InReview (override)`. Repair before the LLM call, not after, so the store is consistent for the full review window.

### API Design

**New public items in `loopr`:**

```rust
// crates/loopr/src/daemon/context.rs
impl DaemonContext {
    /// Triage + review + route. Consumes `bundle` by value because the
    /// task owns it for the LLM call duration.
    pub async fn spawn_reviewer_for_bundle(self: Arc<Self>, bundle: domain::Bundle) -> ();

    /// Integrate + transition downstream Work state. Holds `git_lock`
    /// internally via `IntegratorDeps`.
    pub async fn spawn_integrator_for_bundle(self: Arc<Self>, bundle: domain::Bundle) -> ();
}

// crates/loopr/src/daemon/git.rs   (new, small)
/// Create `loopr/plan-<plan-id>` from target HEAD. Idempotent: if the
/// branch already exists, returns Ok without error. Called from
/// handle_plan_create before Implementer dispatch.
pub async fn ensure_integration_branch(
    target: &std::path::Path,
    plan_id: &domain::PlanId,
) -> Result<(), std::io::Error>;
```

**Signature changes on existing items:**

- `DaemonContext::new` grows three parameters (`reviewer_config`, `integrator_config`, `git_lock`).
- `daemon::run_active_daemon` reads `config.agents.reviewer`, constructs an `IntegratorConfig::default()` (or from `config.integrator`), allocates the `git_lock`, passes them through.
- `spawn_implementer_for_work`'s body gains two additions:
  1. At top: two Coordinator transitions `Pending -> Ready -> InProgress`, guarded against already-correct state (a restart reconcile may have fired them already).
  2. On `Ok(bundle)`: `Role::Implementer` transition `InProgress -> InReview`, then spawn Reviewer.
  3. On `Err`: replace raw `work.status = WorkStatus::Blocked` with `work.transition(WorkStatus::Blocked, Role::Coordinator)`.

**Deletion:** `mark_blocked` function at `crates/loopr/src/daemon/context.rs:320` deleted; its callers go through a new `transition_and_persist_work` helper that uses the FSM.

### Invariants

- **Role-as-identifier extended to Implementer and Integrator.** The Reviewer doc established that `Role::Coordinator` is used as an identifier when the daemon acts as orchestrator, not as a reference to a Coordinator agent. This doc generalizes: the daemon fires `WorkStatus::InProgress -> InReview` using `Role::Implementer` as identifier on successful `run_implementer` return, and `WorkStatus::InReview -> Integrated` using `Role::Integrator` as identifier on successful `integrate` return. The daemon is the trigger; the role identifier names the semantic author of the transition. This preserves the FSM's authored-transitions table (instead of forcing every daemon-initiated edge through the overrides table, which is documented as a bypass mechanism). Future agent instances for Implementer/Integrator are already the default authors; the daemon's use of the same identifier does not require further rework.

- **Every Work-state change goes through `Work::transition`.** No raw `work.status = ...` mutations. The Stage 7 `mark_blocked` bug is fixed as part of Phase 1. The only place in the tree that currently bypasses the FSM is deleted.

- **Integration branch creation is eager AND gates all store writes.** `handle_plan_create` creates `loopr/plan-<plan-id>` BEFORE persisting the Plan or Works. If branch creation fails, no records land on disk; the user sees `RpcError::Internal` and can retry or fix the underlying issue (commonly: target has no HEAD yet). The Integrator doc's Open Question #1 settled here: deterministic base SHA is required by the doc's "same bundles + same base = same Tick SHA" invariant. Architect R1 additionally tightened the ordering to prevent orphan DB records when git fails.

- **Plan births Active, not Draft.** `Plan::new` sets status to `PlanStatus::Active` (per `crates/domain/src/plan.rs:80`). Stage 8 wiring fires no `Draft -> Active` transition; only `Active -> Complete` from the Integrator. If a future stage introduces an interview loop that births `Draft`, the wiring grows one more transition at `handle_plan_create`, not here.

- **Inline dispatch, not events.** Task completion directly enqueues the next stage's task, same as Stage 7. An event bus is in vision's Deferred Enhancements; earn it when polling the JoinSets becomes a bottleneck.

- **Circuit breaker for `Integrating` retries.** `integrate` returning `Update(Stale)` or transient `Store(...)` is retryable; 5 attempts with backoff `[100ms, 500ms, 2s, 5s, 5s]`. On exhaustion, transition the Work to `Blocked`. Non-retryable (`BundleNotAccepted`, `PlanBundleMismatch`, `IntegrationBranchMissing`, `ConflictStructural`, `ConflictRetryable`): break immediately to the Blocked path.

- **Reconcile is a dispatch path, not a repair path.** Every Bundle-FSM sweep decision routes through the same `spawn_*_for_*` methods the handler uses. Reconcile does not mutate Bundle/Work state directly; it just re-enqueues tasks. This keeps a single happy-path through the pipeline code.

- **Shutdown drain order: implementer -> reviewer -> integrator -> watcher -> try_unwrap.** Upstream drains first so new downstream tasks can still land before their stage drains. Each drain has its own soft timeout + `abort_all` fallback (see Phase 4 for budgets).

- **Reviewer/Integrator tasks hold `Arc<DaemonContext>`.** Same pattern as `implementer_tasks`. `Store::close` on the try_unwrap path requires every clone to drop; the three new JoinSet drains are each a holder.

### Implementation Plan

#### Phase 1: Domain edge + Work-FSM orchestration fix + integration-branch creation
**Model:** sonnet

Mechanical: add one FSM edge to domain, fix the raw `work.status =` mutation, add shared transition helpers, wire integration-branch creation (ahead of store writes), acknowledge reconcile ordering shift.

- `crates/domain/src/work.rs`: add one line to the `overrides(...)` macro:
  ```
  InReview   => Blocked by (Coordinator),
  ```
  After `InReview => Ready`. Closes the partial-fail orphan risk on reject; collapses the reviewer/integrator reject paths to one-step FSM calls. Update the `#[derive(Fsm)]` expansion regenerates automatically.
- `crates/loopr/src/daemon/context.rs`:
  - Delete `mark_blocked` (line 320). Replace with two shared helpers:
    ```rust
    async fn transition_and_persist_work(
        store: &Store,
        work: &mut Work,
        target: WorkStatus,
        role: Role,
        override_: bool,   // true => work.override_status(target, role)
                           // false => work.transition(target, role)
    ) -> Result<(), LooprError>

    async fn transition_and_persist_plan(
        store: &Store,
        plan: &mut Plan,
        target: PlanStatus,
        role: Role,
    ) -> Result<(), LooprError>
    ```
    Each uses the appropriate FSM method, then `store.{works,plans}().update(record.clone())`. On FSM rejection logs and returns `Err` so the caller can decide.
  - `spawn_implementer_for_work` body amendments:
    - Before `rev_parse_head`: transition `work: Pending -> Ready -> InProgress` via `Role::Coordinator`. Guard: if the Work is already `InProgress` (restart-reconcile pre-advanced it), skip.
    - On `Ok(bundle)`: transition `work: InProgress -> InReview` via `Role::Implementer`. Reviewer spawn is Phase 2 - Phase 1 just transitions.
    - On `Err`: transition `work: InProgress -> Blocked` via `Role::Coordinator`.
- `crates/loopr/src/daemon/git.rs` (new, small module):
  - `pub async fn ensure_integration_branch(target: &Path, plan_id: &PlanId) -> Result<(), io::Error>`.
  - Implementation: `git -C <target> rev-parse --verify loopr/plan-<id>`; if exit 0, return Ok. Otherwise `git -C <target> branch loopr/plan-<id> HEAD` (no checkout - base SHA is captured by the branch ref).
- `crates/loopr/src/daemon.rs`: re-export `daemon::git` as `pub(crate) mod git`.
- `crates/loopr/src/daemon.rs::run_active_daemon` reconcile ordering shift (acknowledged, not accidental): `reconcile` currently runs at `daemon.rs:278`, BEFORE `Config::load` and `DaemonContext::new`. Phase 4 needs reconcile to spawn into the context's JoinSets, which requires the context to exist. Move the `reconcile` call to AFTER `DaemonContext::new` (post-line 339 in the current file). Telemetry init still runs first (line 255); the substantive change is that `Config::load`, `AnthropicClient` construction, `LaneRouter`/`BashDenylist` construction, and `DaemonContext::new` all run BEFORE reconcile, where previously reconcile ran against a bare `Store`. Safe because reconcile's existing body only needs `target` + `store`, both already available at the new position; the new Bundle-sweep needs the context. This ordering change is explicit, not incidental.
- `crates/store/src/works.rs`: add `WorksStore::list_by_parent_id(&self, plan_id: &PlanId) -> Result<Vec<Work>, StoreError>`. Parallel to `TicksStore::list_by_plan_id` (already shipped). `Work.parent_id` is already `#[record(indexed)]`, so the backing query is an index lookup. Needed by Phase 3's "all Works done -> Plan Complete" check. Test: create 3 Works under plan A and 2 under plan B; assert `list_by_parent_id(A)` returns exactly the 3 A-Works.
- `crates/loopr/src/transport/handler.rs::handle_plan_create`: full reorder so git success gates all store writes (prevents orphan DB records on branch-creation failure):
  1. `let plan = domain::Plan::new(params.goal)` - PlanId now exists in memory, Plan not yet persisted. `Plan::new` births `PlanStatus::Active` per `crates/domain/src/plan.rs:80`; no `Draft -> Active` step in this doc.
  2. `git::ensure_integration_branch(&ctx.target, &plan.id).await`. On Err, return `RpcError::Internal("integration branch creation failed: {e}")` WITHOUT any store writes. No Plan or Works land on disk when git fails, so there are no orphan records to sweep.
  3. `ctx.store.plans().create(plan.clone()).await` - persist Plan only after branch creation succeeded.
  4. `decomposer::decompose(&plan, &ctx.target, &*ctx.llm).await` - existing logic.
  5. `ctx.store.works().create_many(works.clone()).await` - existing logic.
  6. For each Work: spawn into `ctx.implementer_tasks` - existing logic.

  No Plan-status transition is fired here (Plan is already Active). The `Active -> Complete` transition happens in `spawn_integrator_for_bundle`'s Plan-check step (Phase 3).
- Tests:
  - Unit test for `ensure_integration_branch`: idempotent second call is noop.
  - Unit test for `transition_and_persist_work`: FSM rejection produces an Err without persisting.
  - Integration test (`crates/loopr/tests/stage_8_phase_1_work_fsm.rs`): stubbed LLM that returns a scripted Implementer session; after handle_plan_create, read `.loopr/taskstore/works.jsonl` and assert Work reached `InReview`. Integration branch exists.

#### Phase 2: Reviewer dispatch
**Model:** opus

Non-mechanical: Verdict routing, triage-before-review, FSM transition chains, Bundle FSM ordering. Judgment work.

- `crates/loopr/src/daemon/context.rs`:
  - Add `reviewer_config: ReviewerConfig` field to `DaemonContext`.
  - Add `reviewer_tasks: Mutex<JoinSet<()>>` field.
  - `DaemonContext::new` signature grows `reviewer_config`.
  - `pub async fn spawn_reviewer_for_bundle(self: Arc<Self>, mut bundle: Bundle) -> ()`:
    1. Triage: `bundle.transition(BundleStatus::Triaged, Role::Coordinator)`; `ctx.store.bundles().update(bundle.clone(), expected_updated_at)`. On Stale: log and return (another task beat us).
    2. Load Work: `work = ctx.store.works().get(&bundle.work_id).await`. On Err: log and return.
    3. **Work state repair at triage time** (moved here from the Accept branch, per Architect R1): if `work.status == WorkStatus::InProgress`, fire `Coordinator: InProgress -> InReview (override)` via `transition_and_persist_work`. Rationale: the Bundle's existence is proof the Implementer finished persisting; if the Work is still `InProgress`, the Implementer crashed between Bundle-persist and its own transition. Repairing before the 30+s LLM call keeps the store consistent for the full review window, rather than leaving the Work at `InProgress` while the Reviewer runs. Guard against unexpected Work states: if `work.status` is neither `InProgress` nor `InReview`, log `warn!` and return without calling `run_reviewer`.
    4. Build `ReviewerDeps { llm: Arc::clone(&self.llm), store: &self.store, context: Arc::clone(&self.context_builder), config: self.reviewer_config.clone(), target: self.target.clone() }`.
    5. `verdict = agents::run_reviewer(&bundle_after_triage, &work, &deps).await`. `run_reviewer` internally mutates the Bundle further (`Triaged -> Reviewed` or `Triaged -> Rejected`) on a clone via OCC.
    6. Match Verdict:
       - `Accept { .. }`: re-read Bundle from store (it's now at `Reviewed` with a fresh `updated_at`). Transition `Reviewed -> Accepted` via `Role::Coordinator`, persist via `BundlesStore::update(bundle, expected_updated_at)`. Work is already at `InReview` thanks to Step 3's repair. Spawn Integrator - Phase 2 leaves the spawn as a `todo!()` placeholder; Phase 3 fills it.
       - `ChangeRequested { .. }` | `Reject { .. }`: `run_reviewer` already persisted the Bundle at `Rejected`. Re-read Work, fire `Coordinator: InReview -> Blocked (override)` via the new edge from Phase 1. One FSM call, no two-step partial-fail risk.
    7. `ReviewerError::EscalationNeeded(_)` / other Err: same one-step `InReview -> Blocked (override)` as the reject path.
  - `spawn_implementer_for_work` on `Ok(bundle)`: after `Implementer: InProgress -> InReview`, spawn into `ctx.reviewer_tasks`.
- `crates/loopr/src/config.rs`: compose `ReviewerConfig` via `AgentsConfig::reviewer` into the top-level `Config`. Default-fallback path unchanged.
- `crates/loopr/src/daemon.rs`:
  - Add `drain_reviewer_tasks(ctx)` parallel to `drain_implementer_tasks`, budget 30s.
  - Drain order in `run_active_daemon` shutdown: implementer -> reviewer -> (integrator placeholder -> ) watcher -> try_unwrap.
- Tests:
  - Unit test for `spawn_reviewer_for_bundle` with a fake LLM that returns `{"kind":"accept","summary":"LGTM"}`; assert Bundle ends at `Accepted`, Work still `InReview`, integrator-spawn-placeholder recorded.
  - Unit test with `change_requested` verdict; assert Bundle ends at `Rejected`, Work ends at `Blocked`.
  - Unit test with LLM Err; assert Work `Blocked`.

#### Phase 3: Integrator dispatch with circuit-broken retry
**Model:** opus

Non-mechanical: retry policy, backoff, terminal-vs-retryable error classification, Work-FSM transitions after Tick. Judgment work.

- `crates/loopr/src/daemon/context.rs`:
  - Add `integrator_config: IntegratorConfig`, `git_lock: Arc<Mutex<()>>`, `integrator_tasks: Mutex<JoinSet<()>>` fields.
  - `DaemonContext::new` signature grows the three parameters.
  - Define constants:
    ```rust
    const INTEGRATOR_BACKOFF: &[Duration] = &[
        Duration::from_millis(100),
        Duration::from_millis(500),
        Duration::from_secs(2),
        Duration::from_secs(5),
        Duration::from_secs(5),
    ];
    ```
    Five attempts total (initial + 4 retries); circuit breaker counts attempts, not retries, to avoid off-by-one confusion.
  - `pub async fn spawn_integrator_for_bundle(self: Arc<Self>, bundle: Bundle) -> ()`:
    1. Load Work for the Bundle: `store.works().get(&bundle.work_id).await`.
    2. Load Plan: `store.plans().get(&work.parent_id).await`.
    3. Build `IntegratorDeps { bundle_sink: &self.store, works: &self.store, ticks: &self.store, config: self.integrator_config.clone(), target: self.target.clone(), git_lock: Arc::clone(&self.git_lock) }`.
    4. Retry loop. For attempt `i` in `0..INTEGRATOR_BACKOFF.len()`:
       - Call `integrate(&[bundle.clone()], &plan, &deps).await`.
       - On `Ok(tick)`: break.
       - On `Err(IntegrationError::Update(BundleUpdateError::Stale { .. }))` or `Err(Store(_))`: log warn, `tokio::time::sleep(INTEGRATOR_BACKOFF[i])`, continue. Respect shutdown: `tokio::select!` the sleep against `ctx.shutdown_notify.notified()` so shutdown doesn't block on backoff.
       - On any other `Err`: break with terminal error.
    5. Final match:
       - `Ok(tick)`: transition Work `InReview -> Integrated` via `Role::Integrator`, then `Integrated -> Done` via `Role::Coordinator`. Persist both. Then a Plan-level check: list all Works under `work.parent_id` via `store.works().list_by_parent_id(&work.parent_id).await`; if every Work is terminal (`Done | Abandoned | Superseded`) with at least one `Done`, fire `Coordinator: plan Active -> Complete` via a helper `transition_and_persist_plan`. Log `info!(tick_id = %tick.id, plan_complete = ?plan_flipped, "integration succeeded")`.
       - `Err(_)` or circuit-break: one-step `Coordinator: InReview -> Blocked (override)` via the new edge from Phase 1. Log `error!(error = %e, bundle_id = %bundle.id, "integrator terminal; Work Blocked")`. Plan is not transitioned to Complete in this branch; if this was the last non-terminal Work for the Plan, the Plan stays `Active` with no route forward (first-gate acceptance; Director will handle this in a later stage).
- `crates/loopr/src/config.rs`: add `pub integrator: IntegratorConfig` to top-level `Config`, default-fallback.
- `spawn_reviewer_for_bundle::Accept` branch (Phase 2 placeholder) now spawns into `ctx.integrator_tasks`.
- `crates/loopr/src/daemon.rs`:
  - Add `drain_integrator_tasks(ctx)` parallel to the others, budget 10s (git ops are fast).
  - Final drain order: implementer (30s) -> reviewer (30s) -> integrator (10s) -> watcher (2s) -> try_unwrap.
- Tests:
  - Unit test: fake `IntegratorDeps` returning Ok(tick) on first call; assert Work reaches `Done`.
  - Unit test: fake returning Stale twice then Ok; assert retry executed, Work reaches `Done`, sleep-count observed.
  - Unit test: fake returning Stale 5 times; assert Work ends `Blocked`, not `Done`.
  - Unit test: fake returning `ConflictRetryable`; assert no retry (terminal), Work ends `Blocked`.
  - Unit test: shutdown during backoff - start retry loop, fire `shutdown_notify`, assert task exits without completing the backoff sleep and without calling integrate again.

#### Phase 4: Reconcile sweep for Bundle FSM + smoke test
**Model:** opus

Non-mechanical: extending reconcile to sweep a second FSM, spawning tasks from reconcile (before the IPC listener binds), ensuring no race with handler-spawned tasks.

- `crates/loopr/src/daemon/startup.rs`:
  - Extend `ReconcileReport` with four new counters: `reviewers_requeued`, `integrators_requeued`, `bundles_terminal`, `bundles_foreign`.
  - New function `pub async fn sweep_bundles(ctx: &Arc<DaemonContext>) -> Result<BundleSweepReport, LooprError>`:
    - `bundles = ctx.store.bundles().list().await?`.
    - For each bundle:
      - `Proposed`, `Triaged`: spawn reviewer into `ctx.reviewer_tasks`.
      - `Reviewed`: Coordinator transition `Reviewed -> Accepted`, spawn integrator.
      - `Accepted`, `Integrating`: spawn integrator (Integrator is idempotent on re-entry per its own doc).
      - `Merged`, `Rejected`, `IntegrationFailed`, `Superseded`: noop.
  - `reconcile` calls `sweep_bundles` AFTER the existing worktree-hygiene sweep and BEFORE `accept_loop` binds. Existing contract preserved: "hygiene sweep runs BEFORE the accept loop binds so no coordinator session can race."
  - Extend the existing `reconcile` signature from `reconcile(target, store)` to `reconcile(target, ctx: &Arc<DaemonContext>)` so the sweep can spawn into the JoinSets. Call site in `daemon.rs::run_active_daemon` updated to pass the already-constructed `&ctx`.
- `crates/loopr/src/daemon.rs::run_active_daemon`: Phase 1 moved the `reconcile` call from BEFORE `Config::load` to AFTER `DaemonContext::new`. Phase 4 now passes `&ctx` to reconcile so `sweep_bundles` can spawn into the Reviewer and Integrator JoinSets. Final ordering: telemetry init -> Store::open -> ensure_loopr_excludes -> Config::load -> AnthropicClient -> LaneRouter/BashDenylist -> DaemonContext::new -> **reconcile(&target, &ctx)** -> bind_listener -> accept_loop.
- Tests:
  - Unit test in `startup/tests.rs`: seed `.loopr/taskstore/bundles.jsonl` with one row per status, run `sweep_bundles`, assert the right JoinSet got the right number of entries; terminal statuses produced no enqueue.
  - Integration test (`crates/loopr/tests/stage_8_plan_to_tick.rs`): single smoke test exercising the full happy path. Stubbed Anthropic client feeds scripted Implementer (one `write` + one `propose_bundle`) and Reviewer (single `Accept` verdict). Real git on a tempdir target with `git init`. Assertions:
    - `.loopr/taskstore/plans.jsonl` has one row, status `Complete` (Plan born `Active` by `Plan::new`, transitions `Active -> Complete` once the Work finishes).
    - `.loopr/taskstore/works.jsonl` has one row, status `Done`.
    - `.loopr/taskstore/bundles.jsonl` has one row, status `Merged`.
    - `.loopr/taskstore/ticks.jsonl` has one row with `integration_branch = "loopr/plan-<id>"`, non-empty `integration_sha`, one `merge_commits` entry.
    - `git -C <target> log --oneline loopr/plan-<id>` shows the merge commit with two parents.
  - Crash-recovery unit test: populate the store with a Bundle stuck at `Integrating` (simulating crash), boot the daemon via `sweep_bundles`, assert the integrator task fires and reaches `Merged`.

#### Phase 5: Architect R1 review + doc close-out + smoke shipped
**Model:** opus

- Run Architect R1 in Design Review mode against this draft before Phase 1 implementation.
- After Phases 1-4 land and `otto ci` at repo root passes: run Architect R2 in Implementation Audit mode. Fold findings into "Post-Implementation Notes."
- Roadmap update: flip Stage 8's Status to "Complete" once the smoke test passes, and update the Stage 8 row to point at this doc.
- Add `Shipped in: v0.5.X` to this doc once the tag lands.
- Delete the now-redundant Stage 7 JoinSet-scope Open Question (resolved by Phase 3's per-stage JoinSets).

## Alternatives Considered

### Alternative 1: Event bus instead of inline dispatch

- **Description:** Each pipeline stage's task publishes an event on `ctx.events` (`BundleCreated`, `BundleAccepted`, `WorkDone`); background subscribers react. Matches vision's reactive-daemon model more literally.
- **Pros:** Decouples stages; handler returns immediately; multiple consumers per event for future features (TUI subscribers, `loopr watch` CLI, budget-enforcement hooks).
- **Cons:** Adds a second dispatch mechanism alongside the Stage 7 JoinSet pattern. Two patterns means the next engineer has to pick one. Vision explicitly lists "typed event bus" as a Deferred Enhancement (#1 in the list), to be earned when polling becomes a bottleneck.
- **Why not chosen:** Earn the event bus when polling is a measured bottleneck. Stage 7 inline precedent is the right shape for Stage 8.

### Alternative 2: Work-state reactor polling TaskStore

- **Description:** Background task polls `.loopr/taskstore/works.jsonl` + `bundles.jsonl` for state changes; dispatches the right `spawn_*_for_*` task for any Work/Bundle in a dispatchable state.
- **Pros:** Crash recovery is free (next poll picks up stranded records); handler returns immediately; one dispatch path.
- **Cons:** Poll interval vs latency tradeoff; dedupe mechanism needed (don't spawn two reviewers for the same Bundle); heavier to test. Same objection as Stage 7 Alternative 3: "Right thing for Stage 7.5+. Stage 7 goal is a single E2E pass, not production-grade reactivity."
- **Why not chosen:** Matches Stage 7's decision. Startup reconcile covers the crash-recovery case without a continuous poll loop.

### Alternative 3: Per-Plan git worktree for multi-Plan concurrency

- **Description:** Each `loopr/plan-<id>` gets its own sibling worktree; the Integrator's `git_lock` becomes a per-worktree lock, lifting the single-Plan bottleneck.
- **Pros:** Multi-Plan concurrency at the git layer.
- **Cons:** More state (worktree registry entry per Plan), more crash-recovery surface, more disk. First gate has at most one active Plan; `git_lock` suffices. Already catalogued as Alternative 6 in the Integrator doc.
- **Why not chosen:** YAGNI for first gate. Earn when multi-Plan contention is measured.

### Alternative 4: Transition `InProgress -> InReview` via `Role::Coordinator` override

- **Description:** Instead of generalizing "Role X as identifier" to Implementer and Integrator, use the existing `Coordinator: InProgress -> InReview (override)` edge in the overrides table.
- **Pros:** Smaller doc delta; reuses an existing edge.
- **Cons:** The overrides table is documented as a bypass mechanism (explicitly: "a stray `Work::transition(Done, Coordinator)` from `Ready` is a typed error"), not the happy path. Using overrides for every daemon-orchestrated transition inverts the table's meaning. Generalizing the Reviewer's "Role X as identifier" invariant to three roles is a one-paragraph Invariants addition and leaves the transitions table honest.
- **Why not chosen:** Generalization is cheaper than conceptual inversion. Also: Implementer is the FSM-authored author for `InProgress -> InReview`; using Coordinator via override would silently decouple from the FSM's intent.

### Alternative 5: One unified `pipeline_tasks: JoinSet<()>` instead of three

- **Description:** Single JoinSet for all implementer/reviewer/integrator tasks. One drain on shutdown.
- **Pros:** Simpler shutdown sequence. One holder of `Arc<DaemonContext>` instead of three.
- **Cons:** No per-stage budgets. Integrator tasks (fast, non-LLM, deterministic) would share a 30s drain budget with Implementer tasks (slow, LLM-bound). `JoinSet::join_next` can't distinguish stage origins, so logging "which stage's task timed out" is lost.
- **Why not chosen:** The three-stage drain gives better diagnostics and matches the natural stage boundaries.

### Alternative 6: Skip integration-branch creation; integrate fires `IntegrationBranchMissing`, daemon creates and retries

- **Description:** Don't touch git at `handle_plan_create`. Let the Integrator fail on first call with `IntegrationBranchMissing`; the spawn routine catches it, creates the branch, and retries.
- **Pros:** Matches the integrator's existing typed-error path.
- **Cons:** Base SHA becomes non-deterministic (set by whenever the first integrator fires, not when the Plan was filed). Breaks the Integrator doc's "same bundles + same base = same Tick SHA" invariant. A user commit between Plan-start and first-integrate would be incorporated into the Plan's base.
- **Why not chosen:** Deterministic base SHA is load-bearing for the Integrator's contract.

## Technical Considerations

### Dependencies

No new external crates. Workspace-internal dep additions:

- `loopr` gains a dep on `integrator` (was already inherited transitively via `store::BundleUpdateSink`; now a direct import).
- No other crates touched at the Cargo level. All agents/integrator/store shapes already exist.

### Performance

- `ensure_integration_branch` runs once per Plan: one `rev-parse` + at most one `branch` subprocess. Sub-second.
- Each `spawn_*_for_*` task is one `tokio::spawn` + clone of `Arc<DaemonContext>`. Negligible overhead.
- Retry backoff: worst case 12.6s sleep on a single Bundle (100ms + 500ms + 2s + 5s + 5s = 12.6s cumulative before circuit break). Respect shutdown via `tokio::select!` so the daemon isn't wedged for 12.6s at shutdown.
- `sweep_bundles` at startup: one `bundles().list()` call (in-memory after taskstore load) + N spawns. Scales linearly with Bundle count in the store; for first-gate ~1-10 Bundles, microseconds to spawn.
- Three drain budgets at shutdown: 30s + 30s + 10s = 70s worst case before `abort_all`. Matches v3/v4 tolerances for clean shutdown.

### Security

- New git subprocess (`ensure_integration_branch`) uses `tokio::process::Command::arg` per-argument; plan_id is a typed `PlanId` with a constrained character set (5-char base36 suffix + `pl-` prefix). No shell metacharacters.
- No new path-sensitive operations; reconcile sweeps read-only against the already-trusted `.loopr/taskstore/bundles.jsonl`.
- Integrator's `git_lock` shared via `Arc<Mutex>`; the usual tokio Mutex safety applies. Not poisoning-sensitive.

### Testing Strategy

Per `CLAUDE.md` "Seam tests, not only unit tests":

**Unit (per stage):**

- `transition_and_persist_work`: FSM rejection, persistence failure, round-trip.
- `ensure_integration_branch`: idempotent second call.
- `spawn_reviewer_for_bundle`: Accept / ChangeRequested / Reject / LLM-error paths.
- `spawn_integrator_for_bundle`: Ok / Stale-retry-Ok / Stale-circuit-break / terminal-error / shutdown-during-backoff.
- `sweep_bundles`: one case per Bundle status.

**Seam (loopr × store × agents × integrator):**

- `crates/loopr/tests/stage_8_phase_1_work_fsm.rs`: after Phase 1, a full Plan-create drives Work to `InReview`.
- `crates/loopr/tests/stage_8_plan_to_tick.rs`: after Phase 4, full Plan-create drives Work to `Done`, Bundle to `Merged`, Tick present. Stubbed Anthropic, real git. THIS is the Stage 8 exit criterion.

**Crash-recovery:**

- Seed store with Bundles at each intermediate status; boot daemon; assert dispatchable states enqueue the right task and terminal states don't.

**Out of scope:**

- Live Anthropic API (Stage 9).
- Multi-Plan concurrency (Alternative 3).
- Cross-process reconcile (vision's single-daemon-per-target).

### Rollout Plan

Single branch (`v5`), per-phase commits, `otto ci` at repo root after each phase. Single tag bump after Phase 5 (target: `v0.5.X`). No feature flag; the capstone either works or the smoke test fails.

## Acceptance Criteria

All must be true before Stage 8 is marked Complete:

- `domain::WorkStatus` overrides table includes `InReview => Blocked by (Coordinator)`. Existing Fsm-derive tests updated to cover the new edge.
- `ensure_integration_branch` exists; `handle_plan_create` calls it BEFORE any store write; returns `RpcError::Internal` on failure with no orphan Plan/Work records on disk.
- `transition_and_persist_work` and `transition_and_persist_plan` helpers exist; `mark_blocked` deleted; no remaining raw `work.status =` assignments in the tree (`grep -R 'work.status =' crates/` returns zero hits outside FSM impls).
- `WorksStore::list_by_parent_id` exists with a passing round-trip test.
- `Plan::new`-born Plans advance `Active -> Complete` once every child Work under the Plan is terminal with at least one `Done`. (No `Draft -> Active` transition; Plans birth Active.)
- `daemon.rs::run_active_daemon` reconcile ordering: `reconcile` called AFTER `DaemonContext::new`, not before `Config::load`.
- `DaemonContext` carries `reviewer_config`, `integrator_config`, `git_lock`, `reviewer_tasks`, `integrator_tasks`.
- `spawn_reviewer_for_bundle` exists; Verdict routing covers all three variants + Err.
- `spawn_integrator_for_bundle` exists; retry backoff schedule = `[100ms, 500ms, 2s, 5s, 5s]`; circuit breaker at 5 attempts; shutdown-aware sleep.
- `reconcile` extended with `sweep_bundles`; all five intermediate Bundle statuses dispatch correctly; four terminal statuses noop.
- Shutdown drains in order: implementer -> reviewer -> integrator -> watcher -> try_unwrap. Each has its own timeout.
- All phase tests pass; `otto ci` at repo root passes.
- `crates/loopr/tests/stage_8_plan_to_tick.rs` produces a `ticks.jsonl` row with two-parent merge commit visible in `git log` on a stubbed-LLM scenario.
- `WorkStatus::InProgress -> InReview` uses `Role::Implementer` as identifier; `InReview -> Integrated` uses `Role::Integrator`. (Grep-style check.)
- No new `#[allow(dead_code)]`, no `.unwrap()` in production paths (tests excepted), no `dyn` trait objects.
- This doc's `Status` flips to `Implemented` in Phase 5.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Work-FSM rejection at runtime when daemon fires a transition the FSM doesn't allow | Med | High | Every new transition site tested; `transition_and_persist_work` returns Err on FSM rejection so the task path can log + exit cleanly instead of panicking |
| `Role::Implementer` / `Role::Integrator` as identifier (daemon firing) confused with agent authorship in logs | Low | Med | Span fields on `spawn_*_for_*` carry both `role_identifier` and `daemon_orchestrated = true`, so log queries can distinguish |
| Reviewer task enqueues an integrator task before a shutdown signal has propagated, leading to "integrator spawned after shutdown_notify" | Low | Med | `spawn_integrator_for_bundle` first-line checks `ctx.shutting_down`; if set, logs + returns without calling `integrate`. Same guard in reconcile's `sweep_bundles` |
| Circuit breaker too aggressive (5 attempts insufficient for real Stale-race scenarios) | Low | Med | `IntegratorConfig` can grow a `max_retries` field; default 5. Revisit after Stage 9 E2E if Stale races are common |
| Circuit breaker too lenient (retries mask a genuine bug) | Low | Med | Every retry logs `warn!("integrator retry {i} of 5")`; circuit-break logs `error!` with full context. Ops-visible |
| `sweep_bundles` races a handler-driven Implementer that just produced a Bundle | Very Low | Low | Sweep runs BEFORE `accept_loop` binds; no handlers can be running yet. Already the contract for the existing worktree reconcile |
| Shutdown during retry backoff wedges the daemon for up to 12.6s | Low | Med | `tokio::select!` between sleep and `shutdown_notify.notified()`; shutdown cuts the retry loop |
| `InReview -> Blocked` partial-fail orphan (historical; closed by R1) | - | - | Resolved by Phase 1's new `InReview -> Blocked by Coordinator` override edge in `domain::WorkStatus`. One FSM call, no two-step. Row retained as a historical marker of the Architect R1 fix |
| Integration-branch creation races a user's manual branch of the same name | Very Low | Low | `ensure_integration_branch` is idempotent: if the branch exists (regardless of who created it), it's reused. User-branch-collision is a target-owner problem, surfaces as weird merge behavior in the Integrator |
| Stub LLM in smoke test drifts from real LLM output format | Low | Low | The stub feeds literal scripted tool-use JSON; if the format changes in `llm`, the test fails loudly. Stage 9 E2E is the authoritative format check |
| Base SHA "snapshot" drifts between `handle_plan_create` and first integrator call if target advances | Low | Med | The snapshot IS the branch creation at handle_plan_create time: `loopr/plan-<id>` is pinned at that HEAD. Subsequent user commits on main don't affect the integration branch. This is the Integrator-doc's deterministic-base-SHA invariant delivered |
| Multiple Reviewer tasks race on the same Bundle (wiring bug or aggressive reconcile) | Low | Med | Reviewer's intra-daemon Mutex OCC on `BundlesStore::update`; second writer gets `Stale`; task logs and exits. Same guarantee the Reviewer doc baked in |
| Fresh target with no HEAD makes `ensure_integration_branch` fail (`git branch` needs a ref) | Low | Med | `handle_plan_create` returns `RpcError::Internal` surfacing the raw `git` stderr to the client; user commits an initial commit and retries. Documented pre-condition in the `loopr plan` help text (out of scope for this doc but noted) |
| Long reconcile sweep delays Ctrl-C (signal watcher spawns AFTER reconcile) | Low | Low | First-gate Bundle counts are single-digit; worst-case sweep is a handful of spawns + no LLM calls before accept binds. Revisit if Bundle-count growth makes reconcile latency noticeable; optional future fix is to spawn the signal watcher BEFORE reconcile |
| Work stuck at `InProgress` with a Rejected Bundle (Implementer crashed, then user rejected manually via a future tool) | Very Low | Med | `sweep_bundles` handles Bundle-side; Work-only sweep is scoped out (see Non-Goals). Deferred |
| Plan-status stuck (hypothetical post-Active interview loop) | Very Low | Low | Stage 8 does not fire `Draft -> Active` (Plans birth Active per `Plan::new`). A future stage that reintroduces an interview loop will need its own wiring for that edge. N/A in this doc |
| All-Works-Done check races with a just-reconciled Integrator (double Complete transition) | Very Low | Low | `transition_and_persist_plan` uses FSM guard; second `Active -> Complete` errors as "Unchanged" or "not a valid transition from Complete" and is swallowed by the caller |
| `IntegrationBranchMissing` at integrate time gives no machine-readable reason on the Blocked Work | Low | Med | `Work.failure_reason` field does not yet exist in `domain` (Stage 7's reconcile doc flagged it as a prerequisite). Currently the error is only in the daemon log. Out of scope for this doc; tracked in Open Questions. Not blocking for first-gate because Stage 8's happy path does not traverse this error |
| Reconcile re-dispatches `Proposed` / `Triaged` Bundles that previously failed with an Anthropic rate limit, hammering the API on restart | Low | Med | First-gate smoke test uses stubbed LLMs, so rate limits are not exercised; Stage 9 live-LLM E2E is where this would surface. A one-line stagger in `sweep_bundles` (e.g. `tokio::time::sleep(Duration::from_millis(100))` between spawns) is cheap to add later. Not blocking for first-gate; accept the gap and revisit in Stage 9 if it bites |

## Process Observation

Stage 8 is the second consecutive stage to need a capstone wiring doc (Stage 7 was the first). The integrator doc and the Stage 7 wiring doc both raised the meta-observation that per-crate docs can ship Implemented while the stage's cross-crate exit criterion is still unreachable. The wiring capstone is what closes that gap.

A durable fix belongs in `docs/roadmap.md` or the project-wide `CLAUDE.md`, not this doc. Relitigated shape:

- Every roadmap stage whose exit criterion spans multiple crates gets a top-level "wiring" / "capstone" design doc as part of its doc set.
- Stage rows in `docs/roadmap.md` carry a `Status:` field that flips to "Complete" only when the exit criterion is demonstrated against a real run (stubbed or live), not when the last constituent doc lands.
- Design docs replace their roadmap placeholder path with a dated filename + shipped-version annotation on landing, so `roadmap.md` is a live index.

Raised (again) here for completeness; a separate one-shot amendment to `docs/roadmap.md` promotes this to policy without putting process rules in the capstone doc itself.

## Open Questions

- [x] **`InReview -> Blocked` via a direct edge vs. two-step.** Resolved by Architect R1: land the direct override edge in Phase 1. One FSM call, no partial-fail orphan. See Implementation Plan Phase 1 domain edit.
- [ ] **Per-stage config placement.** `reviewer_config` reads `agents.reviewer` (already defined); `integrator_config` needs a new `integrator:` block in `config.yml`. The Stage 6 config layout used `agents.implementer`; should `integrator` become `agents.integrator` (even though `integrator` isn't an agent), or live at the top level? Minor ergonomic question; Phase 3 decides.
- [ ] **Reviewer retry on LLM-transient errors.** Currently, `ReviewerError::Llm(_)` maps to Work Blocked. A future refinement: transient 5xx from the Anthropic API warrants a retry at the daemon layer similar to the Integrator's retry on Stale. First-gate: single-attempt, defer retry until real runs show transient failures are common.
- [ ] **Per-Plan integrator task scope.** One JoinSet per stage (this doc's choice) vs. one JoinSet per Plan (deferred from Stage 7). Multi-Plan opens this up as a real choice; first-gate single-Plan makes it moot. Keeps the Stage 7 Open Question alive.
- [ ] **Implementer retry on Work `Blocked`.** First gate gives up at Blocked. A future doc adds bounded attempt counters and re-enqueues via `Worktree::create` with `seq+1`. Signal: Stage 9 E2E sees legitimate reject-then-succeed patterns.
- [ ] **Budget enforcement.** Vision per-Work / per-run caps. `DaemonContext::can_spawn_new_work()` placeholder. Earn when a runaway pipeline surfaces.
- [ ] **`Work.failure_reason` field on `domain::Work`.** Stage 7's reconcile doc flagged this as prerequisite for machine-readable Work failure diagnostics; Stage 8 inherits the gap. Populating `FailureReason::IntegrationBranchMissing`, `FailureReason::ConflictStructural`, etc. on a Blocked Work (rather than only in the daemon log) is a separate design doc. Trigger: a real run where human users need to know WHY a Work is Blocked without scraping logs.
- [ ] **Reconcile spawn staggering against LLM rate limits.** `sweep_bundles` blindly spawns a task per Bundle in an intermediate state. If a previous run crashed due to Anthropic rate-limit 5xx and the user restarts, the sweep re-spawns immediately, potentially re-triggering the limit. Mitigation is a one-line `tokio::time::sleep(Duration::from_millis(100))` between spawns in `sweep_bundles`. First-gate uses stubbed LLMs; defer until Stage 9 live-LLM runs show it mattering.

## References

- `docs/vision.md`:
  - Line 16: pipeline shape `Goal -> ... -> integrator -> Tick`
  - Line 164: `run_implementer` signature
  - Line 165: `run_reviewer` signature
  - Line 179: `integrate` signature
  - Line 269: `ralph.<role>` span convention (`stage.review`, `stage.integrate`)
  - Line 413: `FailureReason` typed variants
  - Line 417: `catch_unwind` at agent task boundary
  - Line 515: never-push; branch-ownership boundary
  - Line 521: branch naming `loopr/plan-<plan-id>`
  - Lines 593-595: First Gate steps 3-6 (Reviewer -> Integrator -> Tick -> main)
  - Line 597: "No Director unless escalation triggers"
  - Line 609: "one Work at a time until serial proves the shape"
- `docs/roles-and-states.md`:
  - Section "How Reviews Flow": canonical three-actor flow this doc implements
  - Section "Why Coordinator and Director are Both Roles": policy that keeps `Role::Coordinator as identifier` legal
- `docs/roadmap.md` Stage 8: the exit criterion this doc closes
- `docs/design/2026-04-22-reviewer.md`:
  - "Role::Coordinator as identifier" invariant (generalized here to Implementer/Integrator)
  - `run_reviewer` signature + ReviewerDeps shape consumed by `spawn_reviewer_for_bundle`
  - `BundleUpdateSink` OCC contract consumed by triage + `Reviewed -> Accepted` transitions
- `docs/design/2026-04-22-integrator.md`:
  - "Wiring retry contract for `Integrating`" invariant honored by `spawn_integrator_for_bundle`'s retry loop
  - Terminal-error list consumed by the retry-classification match
  - `IntegratorDeps` + `integrate` signature
  - Open Question #1 (integration-branch creation timing) resolved here: at `handle_plan_create`
- `docs/design/2026-04-22-stage-7-wiring.md`:
  - Capstone pattern this doc extends
  - `spawn_implementer_for_work` structure mirrored
  - `Arc<DaemonContext>` handler contract extended to two more JoinSets
  - Stage 7 Process Observation (capstone-as-pattern) revisited here
- `crates/domain/src/bundle.rs:42-58`: Bundle FSM transitions consumed
- `crates/domain/src/work.rs:133-142`: `Work::new` default `Pending` status
- `crates/domain/src/role.rs`: all seven `Role` variants
- `crates/loopr/src/daemon/context.rs`: `DaemonContext` field expansion site; `spawn_implementer_for_work` amendment site
- `crates/loopr/src/daemon/startup.rs`: `reconcile` extension site
- `crates/loopr/src/transport/handler.rs::handle_plan_create`: handler call-site edit
- v3/v4 prior art (reference, not ported): `loopr/src/agents/coordinator.rs` (v3 daemon routing), `loopr-v4/src/daemon/handlers/` (v4 per-handler dispatch). Neither maps cleanly to v5's typed seams; both informed the inline-dispatch choice in Alternative 1.
