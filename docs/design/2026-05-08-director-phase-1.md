# Design Document: Director - Phase 1 (Routine Orchestration)

**Author:** Scott A. Idler
**Date:** 2026-05-08
**Status:** Approved
**Review Passes Completed:** 5/5 + advisor review addressed
**Crates touched:** `domain`, `llm`, `agents`, `context`, `loopr` (daemon wiring), `store`

---

## Summary

Director Phase 1 is a long-lived Opus LLM agent that runs as a per-Plan task in the daemon and provides the LLM-driven orchestration layer that Stage 8's hard-coded inline dispatch cannot: Blocked Work recovery, Reviewed Bundle acceptance policy, and goal completion detection. The daemon's existing reactive dep-gate promotion (1.1) and inline Reviewer→Integrator chain (Stage 8) remain as the primary pipeline; the Director supplements them by polling TaskStore, assembling a state summary via `context::build_for_director`, and issuing typed `DirectorAction` variants for anything the pipeline left in a stuck or recoverable state.

---

## Problem Statement

### Background

Stage 8 wiring (`docs/design/2026-04-22-stage-8-wiring.md`) shipped a deterministic pipeline: Implementer produces Bundle → Reviewer runs automatically → Accepted Bundle spawns Integrator → Work Done. The dep gate (1.1) extends this so only dep-free Works start initially and newly unblocked Works promote reactively when their deps reach Done.

This automatic pipeline handles the happy path. It has no policy surface and no recovery capability:

1. **Blocked Works dead-end.** When a Reviewer rejects a Bundle, Work goes `Blocked`. Nothing reads that state and decides what to do. A real plan with any rejection is permanently stuck.

2. **No acceptance policy.** Stage 8 auto-accepts every `Reviewed` Bundle by transitioning `Reviewed → Accepted` in `spawn_reviewer_for_bundle`. There is no agent that can hold, question, or batch bundles. If the Director needs to run with a different policy in Phase 2 (e.g. batch multi-bundle Ticks), the auto-accept path becomes an obstacle.

3. **No goal-completion audit.** Stage 8 has a `Plan Active → Complete` check inside `spawn_integrator_for_bundle`, but it fires only if the last Integrator runs cleanly. A plan where the last Work goes Blocked rather than Done has no path to completion.

### Problem

The v5 daemon is an implicit, hard-wired Coordinator frozen into the spawn-chain. It cannot recover failures, cannot exercise judgment about what to do next, and cannot be prompted with a different strategy. The Director agent externalises those decisions into an LLM.

### Goals

- `run_director(plan_id, deps)` long-running async function in `crates/agents` that owns per-Plan supervision.
- Typed `DirectorAction` enum covering the Phase 1 orchestration vocabulary.
- State summary assembled via existing `context::build_for_director` contract (implemented in 2.4).
- Poll-based state change delivery at configurable interval; event bus is 3.2.
- Multi-turn history: user = fresh state summary (last message), assistant = action JSON; `trim_history` handles budget.
- Reconciliation sweep before each LLM call: Integrated→Done promotion, GoalComplete check.
- `AcceptBundle` takes over from Stage 8's auto-accept; Stage 8's `spawn_reviewer_for_bundle` stops firing `Reviewed → Accepted` automatically.
- Blocked Work recovery: Director sees `Blocked` Works and emits `OverrideWork { target: Ready }` to retry, or `Done` if no recovery is warranted yet.
- Opus model per vision.md model budget.
- Restart story: 3 restarts on transient failures; `NeedHelp` exits without restart.

### Non-Goals

- **Event bus subscription** (3.2). Phase 1 polls.
- **Director Phase 2 judgment plane** (3.1): pattern tracker, escalation modes, user-intervention chat.
- **Re-decomposition** (2.2). Phase 1 action vocabulary has no `redecompose`.
- **SLA tracking** (2.3). Director reads `attempt_count` from `WorkLine` but has no wall-clock SLA config in Phase 1.
- **Researcher spawning** (2.1). Phase 1 has no `spawn_researcher` action.
- **Parallel Implementers** (3.3). Phase 1 is single-concurrent-Work per dep chain (same as current pipeline).
- **Per-session history persistence.** Director history lives in-memory; TaskStore persistence of LLM turn records is a separate deferred item.
- **Replacing Stage 8's dep-gate reactive dispatch.** `promote_unblocked_siblings` (1.1) stays; Director supplements it.

---

## Proposed Solution

### Overview

The Director is a per-Plan task spawned by `handle_plan_create` alongside the existing pipeline. It does not replace the dep-gate dispatch or the Implementer→Reviewer inline chain. It takes over bundle acceptance and provides recovery for stuck states.

```
handle_plan_create
  │
  │  creates Plan, Works (all Pending)
  │  dep-gate partitions: spawns Implementers for unblocked Works (1.1)
  └─► ctx.director_tasks.spawn(run_director(plan_id, deps))

  ┌── Dep-gate reactive path (unchanged from 1.1) ─────────────────┐
  │  Pending Work → dep resolved → Ready → InProgress (daemon)     │
  │  Implementer done → InReview → spawn_reviewer (Stage 8)        │
  │  Reviewer done → Reviewed  (Stage 8 stops here; no auto-accept) │
  └─────────────────────────────────────────────────────────────────┘

  ┌── Director loop (NEW) ─────────────────────────────────────────┐
  │  1. reconcile sweep: Integrated→Done, GoalComplete check       │
  │  2. build DirectorState (Works + Bundles snapshot)             │
  │  3. build_for_director → AssembledContext                      │
  │  4. llm.complete_free (Opus) → response                        │
  │  5. parse_director_actions → Vec<DirectorAction>               │
  │  6. execute actions:                                           │
  │      AcceptBundle  → Reviewed→Accepted, spawn Integrator       │
  │      OverrideWork  → FSM override (e.g. Blocked→Ready retry)   │
  │      AssignWork    → dep-gate check + spawn Implementer        │
  │  7. append user + assistant messages to history                │
  │  8. sleep poll_interval (idle_interval if no actions taken)    │
  └────────────────────────────────────────────────────────────────┘
```

### Architecture

```
crates/agents/src/director.rs (new)
  run_director<L, S, C, P>(plan_id, deps) -> Result<(), DirectorError>
  DirectorDeps<L, S, C, P> { llm, store, context, spawner, config }
  DirectorConfig { poll_interval_secs, idle_interval_secs, max_restarts, model, token_budget }
  DirectorAction { AcceptBundle, OverrideWork, AssignWork, Done, NeedHelp }
  DirectorError { Llm, Lifeguard, NeedHelp, Store, Parse }

  pub trait WorkSpawner: Send + Sync + 'static {
      fn accept_bundle(&self, bundle_id: BundleId);
      fn override_work(&self, work_id: WorkId, target_status: WorkStatus, reason: String);
      fn assign_work(&self, work_id: WorkId);
  }

crates/loopr/src/daemon/context.rs (amended)
  impl<L: LlmClient + ...> WorkSpawner for Arc<DaemonContext<L>>
  DaemonContext grows: director_tasks: Mutex<JoinSet<()>>, director_config: DirectorConfig

crates/loopr/src/transport/handler.rs (amended)
  handle_plan_create: spawn Director task after dep-gate Implementer spawns
  spawn_reviewer_for_bundle: Accept verdict no longer fires Reviewed->Accepted + Integrator
    Instead: transitions Bundle Reviewed (leaves it there for Director to accept)

crates/loopr/src/daemon/startup.rs (amended)
  startup_reconcile_directors: on daemon boot, lists non-terminal Plans and re-spawns
    Director tasks for any Plan that lacks a live Director (handles daemon restarts and
    Director crashes without daemon restart)

crates/llm/src/client.rs (amended)
  LlmClient::complete_free: add model: Option<&str> parameter
  LlmClient::complete_with_tool: add model: Option<&str> parameter
  AnthropicClient impls: use model override when Some, fall back to self.config.model when None
  Arc<L> forwarding impl: pass model through (impl bound: L: LlmClient + Send + Sync + ?Sized)
  All existing call sites: pass None (no behavior change for Implementer/Reviewer/Decomposer)
  Director call site: pass Some(deps.config.model.as_str())

  Note: as of v0.7.10 the trait uses `#[trait_variant::make(Send)]` over plain
  `async fn`, so the new param is `model: Option<&str>` (not `Option<&'a str>`)
  and impls add the param via `async fn complete_free(&self, system: &str,
  messages: &[Message], model: Option<&str>) -> Result<...>`. The hand-rolled
  `impl Future + Send + 'a` shape and `<'a>` lifetimes were removed in the
  trait-variant cleanup (docs/design/2026-05-08-trait-variant-cleanup.md).

crates/domain/src/work.rs (already shipped in v0.7.10 alongside trait-variant cleanup)
  FSM table: Blocked => Ready by (Reactor, Director)
  FSM table: Integrated => Done by (Reactor, Director)

crates/context/prompts/agents/director/ (amended)
  system.pmt: replace stub with real action vocabulary + role persona
  user.pmt: replace stub with real state-table template
```

### Stage 8 Handoff: Bundle Acceptance

Stage 8's `spawn_reviewer_for_bundle` currently fires `Reviewed → Accepted` on `Verdict::Accept` and immediately spawns an Integrator. Phase 1 transfers that decision to the Director:

**Before (Stage 8 Accept branch):**
```rust
Verdict::Accept => {
    // Re-read Bundle (now Reviewed). Transition Reviewed → Accepted.
    transition_and_persist_bundle(Accepted, Coordinator);
    ctx.integrator_tasks.spawn(spawn_integrator_for_bundle(bundle));
}
```

**After (Phase 1):**
```rust
Verdict::Accept => {
    // Bundle is now Reviewed. Director polls for it and decides to accept.
    // spawn_reviewer_for_bundle does nothing further on Accept.
    info!(bundle_id = %bundle.id, "bundle Reviewed; Director will accept");
}
```

The Director's poll loop sees `BundleStatus::Reviewed`, emits `AcceptBundle { bundle_id }`, and `WorkSpawner::accept_bundle` fires `Reviewed → Accepted` + spawns Integrator. This transfers one step from the daemon's implicit logic to the Director's explicit policy.

`ChangeRequested` and `Reject` verdicts retain their Stage 8 behavior: `Work InReview → Blocked` is still fired by the reviewer's error/reject path. The Director then sees the `Blocked` Work on its next poll and decides whether to retry.

### Data Model

#### DirectorConfig

```rust
pub struct DirectorConfig {
    /// Seconds between iterations when actions were taken.
    pub poll_interval_secs: u64,     // default: 5
    /// Seconds between iterations when no actions were taken.
    pub idle_interval_secs: u64,     // default: 15
    /// Max restarts on transient failure.
    pub max_restarts: u32,           // default: 3
    /// Anthropic model for Director calls.
    pub model: String,               // default: "claude-opus-4-7"
    /// Token budget per LLM call (system + history + state summary).
    pub token_budget: usize,         // default: 100_000
}
```

Composed into `AgentsConfig.director: DirectorConfig` and read from `config.yml agents.director`. Default-fallback path is identical to `AgentsConfig.implementer`.

#### Director lifecycle (no explicit FSM state struct)

The v3 coordinator FSM (Interviewing -> Decomposing -> Planning -> Executing -> GoalComplete) collapses to two cases for v5: the Director is either *running* (polling, issuing actions) or *done* (all Works terminal with at least one Done, exiting `Ok(())`). v5's decomposer runs before the Director starts, so by the time `handle_plan_create` spawns the Director the Plan already has Works in Pending/Ready state.

An earlier draft of this doc proposed a `DirectorFsmState { Executing, GoalComplete }` enum to make the lifecycle explicit. It was scaffolded in Phase 1 then dropped at Phase 2 entry per Architect review: the enum was not persisted, not surfaced to the LLM, and never matched against. The same lifecycle signal is encoded — without ceremony — by `reconcile_director` returning `Result<bool, DirectorError>` where `Ok(true)` means GoalComplete and triggers `run_director`'s `return Ok(())`. The Plan record's status covers the Plan-level persisted lifecycle separately.

#### DirectorAction

```rust
/// LLM-emitted instruction. Serialized as `{"action": "<kind>", ...}`.
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DirectorAction {
    /// Accept a Reviewed Bundle; spawn Integrator.
    AcceptBundle { bundle_id: String },
    /// FSM override on a Work. Primary Phase 1 recovery path.
    OverrideWork {
        work_id: String,
        target_status: String,  // "Ready", "Abandoned", etc.
        reason: String,
    },
    /// Explicit Work assignment. Edge case only: the dep-gate reactive path
    /// (1.1) handles the common case. Director emits this only when it
    /// observes a Ready Work that the reactive path missed (e.g. a dep
    /// resolved after a daemon restart before the promotion sweep ran).
    /// WorkSpawner::assign_work validates dep-gate before spawning; no-ops
    /// silently if deps are not all Done.
    AssignWork { work_id: String },
    /// No actions needed this iteration. NOT a FSM transition; Director
    /// stays in Executing and resumes polling after idle_interval_secs.
    Done { summary: String },
    /// Unrecoverable state; exit immediately. NOT a FSM transition; this
    /// exits the Director task with DirectorError::NeedHelp and no restart.
    NeedHelp { reason: String },
}
```

`target_status` is a plain string parsed to `WorkStatus` by the `WorkSpawner` impl, matching the same pattern used by the existing IPC `work.transition` handler.

#### WorkSpawner trait

```rust
/// Fire-and-forget interface injected into `run_director`.
/// Implemented by `Arc<DaemonContext<L>>` in `crates/loopr`.
pub trait WorkSpawner: Send + Sync + 'static {
    /// Transition Bundle Reviewed → Accepted; spawn Integrator task.
    fn accept_bundle(&self, bundle_id: BundleId);
    /// FSM override on a Work. The impl validates the transition is
    /// permitted before firing.
    fn override_work(&self, work_id: WorkId, target_status: WorkStatus, reason: String);
    /// Dep-gate check + spawn Implementer for a Ready Work. No-op if
    /// the Work's deps are not all Done.
    fn assign_work(&self, work_id: WorkId);
}
```

### API Design

#### `run_director`

```rust
#[instrument(
    name = "director.run",
    level = "info",
    skip_all,
    fields(
        plan_id = %plan_id,
        iteration = tracing::field::Empty,
        restart = tracing::field::Empty,
    ),
    err,
)]
pub async fn run_director<L, S, C, P>(
    plan_id: &PlanId,
    deps: &DirectorDeps<L, S, C, P>,
) -> Result<(), DirectorError>
where
    L: LlmClient,
    S: DirectorStore,
    C: ContextBuilder,
    P: WorkSpawner,
```

`DirectorDeps<L, S, C, P>` mirrors the `Deps<L, T, W, S, C>` struct pattern from the Implementer:

```rust
pub struct DirectorDeps<L, S, C, P> {
    pub llm: L,
    pub store: S,
    pub context: C,
    pub spawner: P,
    pub config: DirectorConfig,
    /// Fires when the daemon is shutting down; Director exits its sleep loop.
    /// Injected from `DaemonContext::shutdown_notify` at spawn time.
    pub shutdown: Arc<Notify>,
}
```

#### DirectorStore

Narrow read-only trait for the two store reads Director needs. Using a narrow trait lets unit tests inject a fake without standing up a full `Store`.

```rust
pub trait DirectorStore: Send + Sync + 'static {
    async fn list_works_for_plan(&self, plan_id: &PlanId) -> Result<Vec<Work>, StoreError>;
    async fn list_bundles_for_plan(&self, plan_id: &PlanId) -> Result<Vec<Bundle>, StoreError>;
}
```

`impl DirectorStore for Arc<Store>`:
- `list_works_for_plan` → `self.works().list_by_parent_id(plan_id)` (added by 1.1).
- `list_bundles_for_plan` → `self.bundles().list_all()` filtered by `work.parent_id`. The `BundlesStore` currently has no `list_by_plan_id`; Phase 1 implementation can add it to `store` or do the in-memory filter. Either is acceptable for first-gate Plan counts. **Decision deferred to Phase 1 implementer; in-memory filter is fine for now.**

### Run Loop

```rust
pub async fn run_director<L, S, C, P>(plan_id, deps) {
    let mut history: Vec<Message> = Vec::new();
    let mut lifeguard = Lifeguard::new();
    let mut restart = 0u32;

    'restart: loop {
        let result: Result<(), DirectorError> = async {
            let mut iteration = 0u32;
            loop {
                // 1. Reconcile sweep
                let goal_done = reconcile_director(plan_id, &deps.store, &deps.spawner).await?;
                if goal_done { return Ok(()); }

                iteration += 1;
                Span::current().record("iteration", iteration);

                // 2. Build state
                let state = build_director_state(plan_id, &deps.store).await?;

                // 3. Context + LLM
                let assembled = deps.context.build_for_director(
                    &state, &history, deps.config.token_budget)?;
                let (response, _) = deps.llm.complete_free(
                    &assembled.system_prompt,
                    &assembled.messages,
                    Some(deps.config.model.as_str()),
                ).await.map_err(DirectorError::Llm)?;

                // 4. Parse
                let actions = match parse_director_actions(&response) {
                    Ok(a) => { lifeguard.reset_parse_failures(); a }
                    Err(e) => {
                        if let Decision::Escalate(r) = lifeguard.record_parse_failure() {
                            return Err(DirectorError::Lifeguard(r));
                        }
                        history.push(Message::assistant(response));
                        history.push(Message::user(format!(
                            "ERROR: Could not parse as JSON array. {e}\n\
                             Respond with ONLY a valid JSON array of action objects."
                        )));
                        continue;
                    }
                };

                // 5. Append turn
                if let Some(msg) = assembled.messages.last().cloned() {
                    history.push(msg);
                }
                history.push(Message::assistant(response));

                // 6. Execute
                let mut took_action = false;
                for action in &actions {
                    match action {
                        DirectorAction::AcceptBundle { bundle_id } => {
                            deps.spawner.accept_bundle(bundle_id.parse()?);
                            took_action = true;
                        }
                        DirectorAction::OverrideWork { work_id, target_status, reason } => {
                            deps.spawner.override_work(
                                work_id.parse()?,
                                target_status.parse()?,
                                reason.clone(),
                            );
                            took_action = true;
                        }
                        DirectorAction::AssignWork { work_id } => {
                            deps.spawner.assign_work(work_id.parse()?);
                            took_action = true;
                        }
                        DirectorAction::Done { .. } => {}
                        DirectorAction::NeedHelp { reason } => {
                            return Err(DirectorError::NeedHelp(reason.clone()));
                        }
                    }
                }

                // 7. Sleep
                let secs = if took_action {
                    deps.config.poll_interval_secs
                } else {
                    deps.config.idle_interval_secs
                };
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
                    // shutdown_notify is injected via DirectorDeps.shutdown: Arc<Notify>
                    _ = deps.shutdown.notified() => { return Ok(()); }
                }
            }
        }.await;

        match result {
            Ok(()) => return Ok(()),
            Err(DirectorError::NeedHelp(r)) => return Err(DirectorError::NeedHelp(r)),
            Err(e) if restart < deps.config.max_restarts => {
                restart += 1;
                Span::current().record("restart", restart);
                warn!(error = %e, restart, "director restart");
                // history.clear() on ALL transient errors, not just parse
                // failures. A restart means the LLM context that led to the
                // error is suspect; feeding it into the next turn risks
                // repeating the same failure. Fresh state on restart is the
                // safer choice; the reconcile sweep re-derives ground truth
                // from the store at the top of every iteration anyway.
                history.clear();
            }
            Err(e) => return Err(e),
        }
    }
}
```

#### Reconciliation sweep

Phase 1 scope: the sweep handles `Integrated -> Done` promotion and the GoalComplete check. It explicitly defers two additional stuck-state cases to Phase 2 (noted below).

```rust
/// Returns true if the Plan is GoalComplete (all Works terminal, at least one Done).
async fn reconcile_director<S: DirectorStore, P: WorkSpawner>(
    plan_id: &PlanId,
    store: &S,
    spawner: &P,
) -> Result<bool, DirectorError> {
    let works = store.list_works_for_plan(plan_id).await?;

    // Integrated -> Done: promote any Work still parked at Integrated.
    // Stage 8's integrator normally fires this inline; the sweep catches
    // crash-interrupted paths.
    for w in works.iter().filter(|w| w.status() == WorkStatus::Integrated) {
        spawner.override_work(w.id.clone(), WorkStatus::Done, "reconcile: Integrated->Done".into());
    }

    // GoalComplete: all terminal, at least one Done.
    let all_terminal = works.iter().all(|w| w.status().is_terminal());
    let any_done = works.iter().any(|w| w.status() == WorkStatus::Done);
    Ok(all_terminal && any_done)
}
```

**Deferred to Phase 2 (not in Phase 1 scope):**

- **Stuck `Triaged` bundles** (Triaged with no live Reviewer task, e.g. daemon crash mid-spawn): Phase 1 has no `spawn_reviewer` on `WorkSpawner`; `WorkSpawner` is deliberately narrow. Phase 2 adds `spawn_reviewer` and expands the sweep.
- **Stuck `Accepted` bundles** (Accepted with no live Integrator task): Phase 1's `WorkSpawner::accept_bundle` is idempotent (no-ops if already Accepted), so re-emitting `AcceptBundle` from the LLM is harmless but requires the LLM to notice the stale Accepted bundle. Phase 2 adds an explicit `Accepted + no-Integrator` check to the sweep and re-fires `accept_bundle` deterministically.
- **Stuck `InProgress` Works** (InProgress with no live Implementer task, e.g. Implementer panicked without FSM cleanup): detecting "no live Implementer" requires checking `implementer_tasks` in `DaemonContext`, which is not reachable via `DirectorStore`. Phase 2 adds a `WorkSpawner::list_running_work_ids() -> Vec<WorkId>` method to expose that state.

The risk row for crash-interrupted stuck states appears in the Risks table. The Phase 2 stub above provides the migration path.

### Prompt Assembly Contract

`crates/context/src/implementer.rs::director_impl` (already implemented in 2.4) renders:
- `agents/director/system.pmt` — fixed persona + action vocabulary (replaced from stub in Phase 4)
- `agents/director/user.pmt` — dynamic state table (rendered from `DirectorUserCtx`)

System prompt per `agents/CLAUDE.md` cache-locality rule: byte-stable across iterations. All iteration-specific data (current Works, current Bundles) lives in the user message via `DirectorUserCtx`.

**system.pmt content** (Phase 4):
- Role: "You are the Director for this Plan. Your job is to accept Reviewed Bundles, recover Blocked Works, and declare GoalComplete when all Works are Done."
- Action vocabulary: one JSON block per variant with field descriptions.
- Rules: respond only with a JSON array; use `done` when no actions are needed; only emit `need_help` for unrecoverable states.
- Dep constraint: only emit `assign_work` for Works whose status is `Ready`; do not assign `Pending` or `Blocked` Works without an `override_work` first.
- Recovery guidance: a Work with `attempt_count < 3` may be retried via `override_work { target_status: "Ready" }`; at 3+ attempts emit `need_help`.

**user.pmt content** (Phase 4):
- Plan ID, Works table (id, title, status, attempt_count, deps visible via status), Bundles table (id, work_id, status).
- Section labels matching what the Reviewer's state-summary labels (e.g. "### Reviewed Bundles (use accept_bundle)") so LLM training signal from similar formats applies.

### Model Selection

`LlmClient::complete_free` and `complete_with_tool` both take a `model: Option<&str>` parameter (added in Phase 0). When `None`, the backend uses `self.config.model` (Sonnet for all existing agents). Director passes `Some(deps.config.model.as_str())` - defaulting to `"claude-opus-4-7"` - so it calls Opus on the same shared `AnthropicClient` instance without a second client or a trait redesign.

All existing call sites (`run_implementer`, `run_reviewer`, `decompose`) pass `None` and observe no behavior change. The `model` field on `DirectorConfig` is the only place in Phase 1 that supplies a `Some` override.

Phase 1 hard-codes Opus per vision.md ("Director: Opus - high-stakes orchestration"). Model-tier resolution (4.2) can replace the literal string with a tier lookup later.

### Daemon Wiring

**`DaemonContext<L>` grows:**
```rust
pub director_config: DirectorConfig,
pub director_tasks: Mutex<JoinSet<()>>,
```

**`handle_plan_create` amendment:**
```rust
// After dep-gate Implementer spawns:
{
    let director_plan_id = plan_snapshot.id.clone();
    let director_deps = DirectorDeps {
        llm: Arc::clone(&ctx.llm),
        store: Arc::clone(&ctx.store),
        context: Arc::clone(&ctx.context_builder),
        spawner: Arc::clone(ctx),  // Arc<DaemonContext<L>>: WorkSpawner
        config: ctx.director_config.clone(),
    };
    ctx.director_tasks.lock().await.spawn(async move {
        if let Err(e) = run_director(&director_plan_id, &director_deps).await {
            error!(error = %e, plan_id = %director_plan_id, "director fatal");
        }
    });
}
```

**`spawn_reviewer_for_bundle` Accept branch amendment:**
Remove `transition_and_persist_bundle(Reviewed → Accepted)` + `spawn_integrator`. The Bundle stays `Reviewed`; Director polls and calls `accept_bundle` via `WorkSpawner`.

**`WorkSpawner` impl on `Arc<DaemonContext<L>>`:**
- `accept_bundle`: re-reads Bundle from store, fires `Reviewed → Accepted`, spawns `integrator_tasks`.
- `override_work`: calls `transition_and_persist_work(override_: true)` with parsed `WorkStatus`.
- `assign_work`: dep-gate check, `Pending → Ready → InProgress`, spawns `implementer_tasks`.

**Shutdown drain order:** implementer → reviewer → integrator → **director** → watcher → try_unwrap. Director budget: 30s (LLM calls possible).

### Implementation Plan

#### Phase 0: Prerequisite crate changes (`domain` + `llm`)
**Model:** sonnet

These changes unblock every subsequent phase. Neither touches agent logic; both are small mechanical edits with high blast radius on call sites.

**Phase 0 status (2026-05-08): partially shipped.** The two `domain` FSM edits below landed in v0.7.10 alongside the trait-variant cleanup. The `llm` `model:` parameter work is still pending and is what an executor of this doc should pick up first. The trait-variant cleanup also reshaped the `LlmClient` trait — see the bullet notes below for the post-cleanup edit shape.

- `crates/domain/src/work.rs`: **DONE in v0.7.10.**
  - ✅ FSM table: `Role::Director` added to the `Blocked => Ready` transition (`by (Reactor, Director)`).
  - ✅ FSM table: `Role::Director` added to the `Integrated => Done` transition (`by (Reactor, Director)`).
  - ✅ `otto ci` green at v0.7.10.
- `crates/llm/src/client.rs`: **PENDING.** As of the trait-variant cleanup (docs/design/2026-05-08-trait-variant-cleanup.md, shipped v0.7.10) the trait uses `#[trait_variant::make(Send)]` over `async fn` and there are no per-method `<'a>` lifetimes. Edits below reflect the post-cleanup shape:
  - Add `model: Option<&str>` as the last parameter to `complete_free` and `complete_with_tool` on the `LlmClient` trait. Methods are plain `async fn`; do not reintroduce `<'a>` or hand-rolled `impl Future`.
  - Update the `Arc<L>` forwarding impl to pass `model` through. The forwarding bound today is `L: LlmClient + Send + Sync + ?Sized`; preserve it.
  - Update `AnthropicClient::complete_free` and `complete_with_tool`: use `model.unwrap_or(&self.config.model)` when building the request body.
  - Update all existing call sites in `agents` and `decomposer` to pass `None` (mechanical, no behavior change).
  - Update stub (`ScriptedLlm` in `crates/llm/src/stub.rs`) and metered (`MeteredLlmClient` in `crates/llm/src/metered.rs`) impls to accept and ignore the new param.
  - Update test fakes that impl `LlmClient`: `FakeLlm` in `agents/src/implementer/tests.rs`, `FakeLlm` in `agents/src/reviewer/tests.rs`, `GatedLlm` in `agents/tests/seam_reviewer_concurrency.rs`. All use plain `async fn` post-cleanup; just add the param to each.
  - `otto ci` at repo root must pass before continuing.
- Tests:
  - `crates/domain`: existing FSM tests already confirm Director role is permitted for those two transitions (verified at v0.7.10).
  - `crates/llm`: add one unit test asserting that passing `Some("claude-opus-4-7")` to a mock server call sends that model string in the request body.

#### Phase 1: Scaffolding (DirectorAction + DirectorConfig + WorkSpawner trait)
**Model:** sonnet

**Phase 1 status (shipped 2026-05-08, commit `f8333f6`).** Audited by Architect against the bullets below: 100% complete with one sensible deviation (`DirectorConfig` lives in `crates/agents/src/config.rs` alongside `AgentsConfig` rather than in `director.rs`; this is the correct long-term placement and matches `ImplementerConfig`/`ReviewerConfig`).

Phase 1 also created two scaffolding types that Phase 2 should DELETE on entry:
- `agents::director::DirectorState` (domain-typed `Vec<Work>`/`Vec<Bundle>`) duplicates the existing `context::DirectorState` (display-typed `Vec<WorkLine>`/`Vec<BundleLine>`). Per the Architect: only one state struct is warranted, and it lives in `context`. Phase 2's `build_director_state` returns `context::DirectorState` directly. Drop the agents-side struct and its `pub use` from `lib.rs`.
- `agents::director::DirectorFsmState { Executing, GoalComplete }` is currently dead — not persisted, not in the LLM prompt, not driving any `match`. The reconcile sweep returning `bool` already encodes the GoalComplete signal. Drop the enum and its `pub use` from `lib.rs`.

- `crates/agents/src/director.rs` (new, single-word file per `rules/general.md`):
  - `DirectorAction` with `#[serde(tag = "action", rename_all = "snake_case")]`
  - `DirectorConfig` with the five fields above
  - `WorkSpawner` trait
  - `DirectorError` enum with `thiserror`
  - `parse_director_actions(response: &str) -> Result<Vec<DirectorAction>, ParseError>`
  - Export: add `pub use director::{DirectorAction, DirectorConfig, DirectorDeps, DirectorError, WorkSpawner, run_director}` to `crates/agents/src/lib.rs`
- Unit tests in `crates/agents/src/director/tests.rs`:
  - `DirectorAction` serde round-trip for all variants
  - `parse_director_actions`: happy path, unknown action key tolerated, malformed JSON errors

#### Phase 2: `run_director` loop + reconcile sweep
**Model:** opus

Non-mechanical: loop structure, history management, reconcile correctness, goal-complete detection, restart logic, lifeguard wiring.

- Cleanup from Phase 1 scaffolding (do this first):
  - Delete `agents::director::DirectorState` struct and remove from `pub use director::{...}` in `crates/agents/src/lib.rs`. The `build_director_state` function below returns `context::DirectorState` directly.
  - Delete `agents::director::DirectorFsmState` enum and remove from `pub use`. Phase 2's loop encodes the FSM implicitly via `reconcile_director`'s `Result<bool, _>`.
- `crates/agents/src/director.rs`:
  - `build_director_state(plan_id, &store) -> Result<context::DirectorState, DirectorError>` is the conversion seam between domain records (`store.list_works_for_plan` / `list_bundles_for_plan`) and the display-oriented `context::DirectorState` (`Vec<WorkLine>`, `Vec<BundleLine>`). All `WorkStatus` / `BundleStatus` values are stringified at this seam so `context` does not import `domain` FSM enums.
  - `reconcile_director(plan_id, &store, &spawner) -> Result<bool, DirectorError>`
  - `run_director` full implementation
  - Lifeguard wired identically to Implementer's pattern
- `crates/agents/src/director/tests.rs`:
  - `run_director` with fake LLM returning `[{"action":"done","summary":"..."}]`: loop runs and sleeps.
  - `run_director` with fake returning `[{"action":"accept_bundle","bundle_id":"bd-xxx"}]`: `spawner.accept_bundle` called.
  - `run_director` where all Works are Done on first reconcile sweep: exits Ok without LLM call.
  - `run_director` with 3 consecutive parse failures: lifeguard escalates.
  - `run_director` `NeedHelp`: exits `DirectorError::NeedHelp` immediately, no restart.
  - `reconcile_director` with Integrated Work: `override_work(Done)` called.

#### Phase 3: Daemon wiring + Stage 8 accept handoff
**Model:** opus

Non-mechanical: removing Stage 8's auto-accept, wiring WorkSpawner, handle_plan_create amendment.

- `crates/loopr/src/daemon/context.rs`:
  - Add `director_config: DirectorConfig`, `director_tasks: Mutex<JoinSet<()>>` fields.
  - Implement `WorkSpawner for Arc<DaemonContext<L>>`: all three methods with shutdown guard.
  - `DaemonContext::new` signature grows two parameters.
- `crates/loopr/src/transport/handler.rs`:
  - `handle_plan_create`: spawn Director task after dep-gate Implementer spawns.
- `crates/loopr/src/daemon/context.rs::spawn_reviewer_for_bundle`:
  - Accept branch: remove `Reviewed → Accepted` + Integrator spawn. Add `info!` log.
- `crates/loopr/src/daemon.rs`:
  - Add `drain_director_tasks(ctx, 30s)`.
  - Drain order: implementer -> reviewer -> integrator -> director -> watcher -> try_unwrap.
- `crates/loopr/src/daemon/startup.rs` (or equivalent reconcile site on daemon start):
  - `startup_reconcile_directors(ctx)`: called during daemon startup after loading TaskStore. Lists all Plans whose FSM state is non-terminal (Active / any state without a Done/Abandoned/Superseded terminal marker). For each such Plan, checks whether `director_tasks` already has a live task (it won't on fresh start). Spawns `run_director(plan_id, deps)` for each. This ensures a Director crash or daemon restart does not permanently stall a Plan.
  - AC: after a daemon restart with an in-flight Plan, the Director resumes polling without manual intervention.
- `crates/loopr/src/config.rs`: add `agents.director: DirectorConfig`.
- Tests:
  - Unit: `WorkSpawner::accept_bundle` transitions Bundle + spawns Integrator task.
  - Unit: `WorkSpawner::assign_work` with dep-blocked Work: no Implementer spawned.
  - Integration (`crates/loopr/tests/stage_9_director_plan_to_tick.rs`): Director stub receives scripted Opus response (`accept_bundle` for the first Reviewed Bundle), Integrator runs, Work Done, Director reconcile detects GoalComplete, task exits. Stubbed Anthropic; real store.

#### Phase 4: Prompt content + instrumentation + CI gate
**Model:** sonnet

- Replace `crates/context/prompts/agents/director/system.pmt` stub with real content.
- Replace `crates/context/prompts/agents/director/user.pmt` stub with structured state table.
- Verify existing `context.build_for_director` span fields still match: `role = "director"`, `plan_id`, `history_len`, `token_budget`, `system_chars`, `token_estimate`.
- Acceptance test for Director span in `crates/context/tests/instrumentation.rs` already passes (from 2.4); confirm no regression.
- `otto ci` at repo root passes.
- `docs/deferred-roadmap.md` §1.2 entry: update proposed filename to actual, add `Status: Implemented`.

---

## Alternatives Considered

### Alternative 1: Director replaces Stage 8's inline dispatch entirely

- **Description:** `handle_plan_create` spawns Director and nothing else; Director dispatches every Work via `AssignWork`, owns every bundle acceptance, owns every Integrator spawn.
- **Pros:** Director owns the full orchestration plane; cleaner single point of control.
- **Cons:** Stage 8's tested pipeline must be dismantled. Risky regression. The dep-gate reactive path (1.1) was specifically designed to be daemon-level, not LLM-level. Shipping Phase 1 as a rip-and-replace multiplies test surface unnecessarily.
- **Why not chosen:** Phase 1 is a supervision layer. Phase 2 can promote Director to full controller when the recovery judgment is battle-tested.

### Alternative 2: Director polls on event bus (no polling sleep)

- **Description:** Director subscribes to `DaemonEvent` broadcast and wakes only on relevant transitions.
- **Pros:** Zero idle overhead; sub-second reaction time.
- **Cons:** Requires 3.2. Not a valid dependency for Phase 1.
- **Why not chosen:** 3.2 is listed as Phase 1's dependency in the forward direction (Phase 1 is a prerequisite for 3.2). Polling is the correct first-gate choice.

### Alternative 3: Daemon keeps Stage 8 auto-accept; Director only handles Blocked

- **Description:** Stage 8's `Reviewed → Accepted` auto-accept stays; Director handles only Blocked recovery.
- **Pros:** Smaller Stage 8 change.
- **Cons:** Director cannot exercise bundle acceptance policy (even Phase 1 should own this). Phase 2's batching policy requires Director to hold bundles — if Stage 8 auto-accepts them all, Phase 2 has to undo that wiring anyway.
- **Why not chosen:** Transferring acceptance to Director in Phase 1 is the right move; it's one targeted Stage 8 change with a clean migration.

### Alternative 4: `WorkSpawner` returns `Result`; Director awaits completion

- **Description:** `accept_bundle`, `assign_work`, `override_work` are async and return `Result`.
- **Pros:** Director can observe action outcomes and react within the same iteration.
- **Cons:** Requires Director to `.await` Implementer/Integrator completion, turning it into an execution supervisor rather than a decision agent. Implementers run for minutes; Director would block for minutes per iteration.
- **Why not chosen:** Fire-and-forget is the right model. Director's next-turn state snapshot reflects the outcomes of previous actions. The polling loop provides eventual observation.

---

## Technical Considerations

### Dependencies

No new external crates. Cargo-level:
- `crates/agents/Cargo.toml`: already has `llm`, `context`, `domain`; add `store` if not already present (Director needs `DirectorStore` type).
- `crates/loopr/Cargo.toml`: already has `agents` dep; no additions.

### Performance

- Opus poll at 5s interval: 1 LLM call per ~8-15s during active execution. First-gate Plans (3-8 Works) complete in a handful of Director turns.
- `build_director_state`: 2 store reads per iteration (list_works, list_bundles); in-memory after taskstore load, microseconds.
- History trimming: `build_for_director` caps at `token_budget` via `trim_history`. At 100k tokens, 1k system + ~3k state/turn = ~30 full turns before trimming. First-gate Plans hit GoalComplete well before turn 30.
- Idle interval (15s) when Director emits `done` with no actions: avoids burning Opus calls when the pipeline is progressing normally.

### Security

No new attack surface. `WorkSpawner::assign_work` validates that the `work_id` parses to a valid `WorkId` and the dep gate is satisfied before spawning. `override_work` validates the FSM transition is permitted before firing.

### Testing Strategy

**Unit (`crates/agents/src/director/tests.rs`):**
- All `DirectorAction` variants serde round-trip.
- `parse_director_actions` happy/unknown/malformed paths.
- `reconcile_director`: Integrated Work triggers `override_work(Done)`.
- `is_goal_complete` (via reconcile): all Done = true; mixed = false; all Abandoned (no Done) = false.
- Full `run_director` with fake LLM and fake WorkSpawner: happy path, NeedHelp, parse-failure lifeguard, Blocked retry.

**Seam (`crates/loopr/tests/stage_9_director_plan_to_tick.rs`):**
- Stubbed Anthropic feeds Director one `accept_bundle` action; rest of pipeline is real (store, FSM transitions, Integrator with stubbed LLM). Assert: Work Done, Bundle Merged, Tick present, Director task exited.

**Regression:**
- Existing `stage_8_plan_to_tick.rs` must continue passing; Director is additive in Phase 3.

### Rollout Plan

Single branch (`v5`), per-phase commits, `otto ci` after each phase. One version bump after Phase 4. No feature flag: Phase 3's accept-handoff is a clean cut; the Stage 9 seam test is the gate.

---

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Stage 8 accept handoff regression: Bundle stuck at Reviewed, Director not yet deployed | Med | High | Phase 3 adds Director spawn and removes auto-accept in the same commit. Integration test validates end-to-end before merge. |
| Director polls idle interval (15s) but Reviewed Bundle arrives immediately after sleep starts | Low | Med | Acceptable latency for Phase 1. Event bus (3.2) eliminates it in Phase 2. |
| Opus cost for a Plan that rarely has Blocked/Reviewed states | Low | Low | Idle interval throttles cost to 1 call per 15s. A 10-Work Plan costs ~40 Opus calls total. Acceptable at first gate. |
| LLM hallucinates `assign_work` for a dep-blocked Work | Low | Med | `WorkSpawner::assign_work` checks dep gate before spawning; no-ops silently with a `warn!` log. |
| Director retries a Blocked Work indefinitely | Low | High | system.pmt cap: retry only if `attempt_count < 3`; at 3 emit `need_help`. `DirectorError::NeedHelp` exits the Director task with an `error!` log. |
| `reconcile_director` fires `override_work(Done)` on an Integrated Work at the same time Stage 8's Integrator fires it inline | Very Low | Low | `transition_and_persist_work` uses OCC; second transition errors as Stale/invalid and is swallowed. WorkSpawner impl must swallow Stale gracefully. |
| Shutdown: Director sleeping 15s blocks drain for 15s | Low | Med | Sleep is `tokio::select!` against `deps.shutdown.notified()`; shutdown cuts the sleep immediately. |
| History blows past `token_budget` on a very long-running plan | Low | Med | `build_for_director` calls `trim_history`; oldest arcs dropped. Tested in 2.4. |
| Zero Works in a Plan: `reconcile_director` returns `false` (no `any_done`); Director enters LLM loop on empty state | Very Low | Low | `run_director` checks `works.is_empty()` at reconcile; if true, logs `warn!` and returns `Ok(())`. The decomposer always produces at least one Work; this is a programmer-error guard. |
| `accept_bundle` called on a Bundle already at Accepted (Director restart clears history, re-emits same action) | Low | Med | `WorkSpawner::accept_bundle` re-reads Bundle status before firing transition; if already Accepted, no-ops with `debug!` log. Prevents double-transition. |
| Reviewer finishes Accept verdict before Director spawns (narrow spawn race) | Very Low | Low | Director spawns synchronously in `handle_plan_create` before the Implementer even starts (decomposer runs first, then both Director and Implementers spawn). Bundle cannot be Reviewed before Implementer completes; Director is always polling before any Bundle can reach Reviewed. |
| Director crashes or hangs during LLM call: all Reviewed Bundles stuck at Reviewed, no Integrators spawn | Med | High | Director has `max_restarts` (default 3) on transient failures. `daemon::startup::reconcile` re-spawns a Director for every non-terminal Plan on daemon boot (Phase 3). `director.run` span emits `error!` on unrecoverable exit so monitoring catches it. Stuck Bundles are visible in `loopr logs` or store query. Phase 2 adds stuck-bundle detection to the reconcile sweep as a deterministic recovery path. |
| Stuck `Accepted` or `Triaged` bundles after Director crash (Phase 2 deferred): Director sees them but LLM must notice | Low | Med | Accepted Phase 1 limitation. Reviewed bundles are the critical path; Accepted (no Integrator) and Triaged (no Reviewer) require Phase 2 sweep expansion. Risk accepted for first gate. |

---

## Open Questions

- [ ] **`list_bundles_for_plan` accessor.** `WorksStore::list_by_parent_id` exists (added by 1.1). A matching `BundlesStore::list_by_plan_id` is needed (Works are parent of Bundles, so an extra join on `work_id → parent_id` is required). Alternatively, Director calls `list_all_bundles` and filters in-memory. Resolve in Phase 1.
- [ ] **`WorkStatus::is_terminal()` definition.** `is_goal_complete` requires knowing which WorkStatus variants are terminal. `Done`, `Abandoned`, `Superseded` are terminal; `Blocked` is NOT (it is recoverable). Confirm in `domain::WorkStatus` before Phase 2 implementation.
- [ ] **`system.pmt` attempt_count threshold.** The retry cap of 3 is hardcoded in the prompt. Should it be `DirectorConfig.max_work_retries`? Resolve in Phase 4 when prompt is authored.
- [x] **Director access to `shutdown_notify`.** Resolved: `DirectorDeps.shutdown: Arc<Notify>` field, injected from `DaemonContext::shutdown_notify` at spawn time.
- [ ] **`DirectorStore` trait vs reuse of `Arc<Store>`.** Implementing a separate narrow trait is cleaner for testing (fake store in unit tests) but requires a new impl on `Arc<Store>`. Decide whether to use a dedicated trait or the existing store interface. Phase 1 may use `Arc<Store>` directly with a `DirectorStore` alias if the existing accessor coverage is sufficient.

---

## References

- `docs/deferred-roadmap.md` §1.2: source material and acceptance criteria
- `docs/design/2026-05-08-multi-turn-llm.md`: `build_for_director`, `DirectorState`, multi-turn Message types (2.4)
- `docs/design/2026-04-22-stage-8-wiring.md`: Stage 8 inline dispatch that Phase 3 amends
- `docs/design/2026-05-07-dependency-gate.md`: dep-gate reactive promotion that Director's `AssignWork` respects
- `docs/design/2026-05-08-validation.md`: validation results that Director sees in state summary
- `docs/vision.md`: "Director: Opus" model budget; "Deferred Enhancements" list
- `crates/context/src/lib.rs:89-172`: `DirectorState`, `WorkLine`, `BundleLine`, `build_for_director` signature
- `crates/agents/src/implementer.rs`: run loop pattern Director mirrors
- `crates/agents/src/lifeguard.rs`: Lifeguard pattern for parse-failure escalation
- `crates/loopr/src/daemon/context.rs`: `DaemonContext` field expansion site
- `crates/loopr/src/transport/handler.rs:141-220`: `handle_plan_create` amendment site
- v3 `~/repos/scottidler/loopr/src/agents/coordinator.rs`: reference FSM, state summary, run loop pattern
- v3 `~/repos/scottidler/loopr/src/agents/coordinator/run.rs`: `run_fsm_loop` polling + event-wake pattern
