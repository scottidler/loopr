# Design Document: Director Phase 1 Follow-ups

**Author:** Scott Idler
**Date:** 2026-05-09
**Status:** Implemented
**Crates touched:** domain, agents, loopr
**Review Passes Completed:** 4/4 (+ Architect Round 2 on attempt_count)

## Summary

Director Phase 1 shipped in v0.7.11 with three known gaps: an untested cold-boot reconcile path, detached `tokio::spawn` tasks inside `WorkSpawner`, and a retry-budget cap that lives only in the LLM's system prompt. This doc closes all three plus a fourth gap discovered during pre-design review (Architect Round 1, Q5): without a Plan-level "stalled" marker, exhausting the retry budget creates a cold-boot death loop. Five phases land the work in the order the dependency graph requires.

## Problem Statement

### Background

Director Phase 1 (v0.7.11, commits `90e5bb4` + `91c7981`) introduced per-Plan Opus orchestration. The shipped surface:

- `agents::run_director` polls TaskStore via the narrow `DirectorStore` trait.
- `WorkSpawner` is the fire-and-forget surface for the Director's actions (`accept_bundle`, `override_work`, `assign_work`).
- Concrete impl is `DaemonSpawner<L>` in `crates/loopr/src/daemon/context.rs` — a thin newtype around `Arc<DaemonContext<L>>` that exists to satisfy the orphan rule.
- Stage 8's `Verdict::Accept` no longer fires `Reviewed → Accepted` inline; the Bundle stays Reviewed for the Director to claim. Bundle FSM extended: `Reviewed => Accepted by (Reactor, Director)`.
- Daemon shutdown drain order: implementer → reviewer → integrator → director → watcher → try_unwrap.

Per `~/.claude/projects/-home-saidler-repos-scottidler-loopr/memory/project-director-phase-1-shipped.md`, three follow-ups were flagged as "NOT ship-blockers" but never closed.

### Problem

Four concrete gaps:

1. **`startup_reconcile_directors` is untested.** It exists at `crates/loopr/src/daemon/startup.rs:362` and was an explicit AC of the Phase 1 design doc, but no integration test exercises the daemon-restart-with-Active-Plan path. Stage 9 covers the in-process `handle_plan_create` spawn; the cold-boot reconcile path is asserted nowhere.

2. **`WorkSpawner` methods spawn detached tasks.** Inside `DaemonSpawner::accept_bundle`, `override_work`, `assign_work`, each method calls `tokio::spawn(...)` without joining the resulting `JoinHandle` into any tracked `JoinSet`. The current mitigation is a load-Relaxed shutdown check at task body entry (`ctx.shutting_down.load(...)` before any DB mutation). If `harness::shutdown` ever logs "DaemonContext Arc still shared (strong_count=N)", this is the suspected path.

3. **The `attempt_count < 3` retry cap lives in `system.pmt` only, not in code.** The Director's prompt instructs the model to give up after 3 attempts on a Blocked Work, but `run_director` does not enforce it. A misbehaving model that ignores the cap loops indefinitely on a deadlocked Work.

4. **No Plan-level "stalled" marker.** When a Director gives up on a Plan (currently via `DirectorError::NeedHelp`), the Plan status remains `Active`. On daemon restart, `startup_reconcile_directors` lists every Active Plan and respawns a Director for each. The new Director immediately reads the same exhausted state, gives up again, and exits. Repeat across every restart — a cold-boot death loop. This gap was caught in Architect Round 1 (Q5) during the pre-design consultation for this doc.

### Goals

1. Every gap above is closed with code + tests, with the dependency order respected (Stalled marker before retry-cap enforcement before reconcile-test, since each builds on the previous).
2. The shutdown drain ordering rationale is documented inline in `crates/loopr/src/daemon.rs` so the next reader doesn't have to re-derive whether `JoinSet::spawn` during a concurrent `join_next` orphans tasks. Same content lives in this doc as a section the comment references.
3. `DirectorConfig.max_work_attempts: u32` is operator-tunable via `.loopr/config.yml` `agents.director.max-work-attempts`, defaulting to 3 (matches the prompt cap).

### Non-Goals

- **Phase 2 of the broader Director roadmap (stuck-state detection beyond Integrated → Done).** That's its own work; this doc only closes the four Phase-1 gaps.
- **Operator UX for clearing a `Stalled` Plan.** The marker is set; clearing it manually is out of scope (operator runs `loopr plan override <id> --to active` when ready, using the existing override path). A dedicated CLI verb can land later if friction warrants.
- **Restructuring `WorkSpawner` to be synchronous.** The fire-and-forget model is correct for Phase 1; we add a tracked JoinSet so shutdown is clean, not change the contract.
- **Lifeguard generalization.** The existing `max_repeat_action`, `max_parse_failures`, `max_requeries` machinery is intra-iteration. `max_work_attempts` is cross-iteration (across Director loop ticks). They don't fit the same shape; we add a new dimension rather than pretending it generalizes.

## Proposed Solution

### Overview

Five phases, with a hard dependency from Phase 1 → Phase 2 → Phase 4. Phase 3 is independent.

| Phase | Scope | Depends on |
|---|---|---|
| 1 | `PlanStatus::Stalled` variant + FSM transitions + `startup_reconcile_directors` skip rule | — |
| 2 | `startup_reconcile_directors` integration tests (cold boot finds Active Plans, Stalled Plans skipped) | Phase 1 |
| 3 | `work_spawner_tasks: Mutex<JoinSet<()>>` on `DaemonContext`; `DaemonSpawner` methods spawn into it; new `drain_work_spawner_tasks` step | — |
| 4 | `DirectorConfig.max_work_attempts: u32` (default 3) + enforcement in `run_director`; on exhaustion, transition `Plan → Stalled` then return `DirectorError::NeedHelp` | Phase 1 |
| 5 | Inline drain-ordering rationale in `daemon.rs` comments; CLAUDE.md update; design doc → Implemented; bump | All |

### Architecture

**Plan FSM extension.** Add `PlanStatus::Stalled`. Transitions:
- `Active → Stalled` allowed for role `(Director)` — fired when retry budget exhausts.
- `Stalled → Active` via `override_status` for role `(Director)` — operator-initiated recovery (using existing override path; no new verb).
- `Stalled` is **non-terminal** by FSM definition (currently `Complete`, `Superseded`, `Abandoned` are terminal). It's a quiescent state: the Director task exits, but the Plan record persists with diagnostic information.

**`startup_reconcile_directors` skip rule.** The filter `plans.into_iter().filter(|p| p.status == PlanStatus::Active)` already excludes `Stalled` (since `Stalled != Active`). Phase 1 is mostly definitional; the existing filter does the right thing for free. We assert this with a Phase 2 test that proves a Stalled Plan does not get a respawned Director.

**WorkSpawner JoinSet.** Add `work_spawner_tasks: Mutex<JoinSet<()>>` to `DaemonContext` next to `director_tasks`. `DaemonSpawner::accept_bundle`, `override_work`, `assign_work` each acquire the lock and `spawn` into the new JoinSet instead of bare `tokio::spawn`. The shutdown_check at task body entry stays — it's defensive for the case where `shutting_down` is set between `lock()` and the spawned task's first poll.

**Drain ordering with `work_spawner_tasks`.** New order: implementer → reviewer → director → work_spawner → integrator → watcher → try_unwrap. The rationale section below explains why the current `integrator → director` order works defensively but is logically muddled, and why the new order reflects spawn-chain semantics directly.

**Retry-budget enforcement (two layers).** Phase 4 adds *both* an increment site and a cap check, in two layers. **Critical context:** in v5, `attempt_count` is currently a dead field — declared on `Work`, initialized to 0 in `Work::new`, read by Director and Implementer prompts, but **never incremented anywhere in production code**. The v3/v4 mechanism (per project memory `project-doom-loop-fix.md`) incremented it "whenever Work is reset to Ready from a non-Draft state" with a `MAX_WORK_ATTEMPTS = 5` spawner-layer hard cap; that mechanism did not carry over to the v5 clean-break rewrite. Phase 4 restores it.

**Layer 1 — increment site (universal chokepoint).** In `transition_and_persist_work` (`crates/loopr/src/daemon/context.rs`), on any successful Work transition where the new status is `WorkStatus::Ready`, increment `work.attempt_count` BEFORE persist. The increment fires for **any** path to Ready: initial `Pending → Ready` dispatch (first attempt) AND `Blocked → Ready` retries (subsequent attempts). This makes the count **1-based**: a Work that has run once has `attempt_count = 1`. With cap=3 (the default), 1-based counting means the cap fires when the Director tries to issue a fourth `→Ready` while the Work already has `attempt_count = 3`. (Architect Round 2 catch: 0-based counting with "increment on Blocked→Ready only" would have allowed 4 total runs, not 3.)

**Layer 2 — Director-layer soft cap (Plan transition + NeedHelp).** The retry path is `DirectorAction::OverrideWork { target_status: "Ready", ... }` — see `daemon/context.rs:1262-1264` ("The Director's primary recovery path is `Blocked -> Ready` to retry a previously-rejected Bundle"). `AssignWork` is initial-or-post-dep-resolution dispatch and is gated by the same Layer-1 increment (its work-eligibility check feeds `transition_and_persist_work` indirectly). Inside `run_director_inner`'s `DirectorAction::OverrideWork` arm, after parsing `target_status` into `WorkStatus` via `parse_work_status`, before calling `spawner.override_work(...)`:

1. If parsed `target == WorkStatus::Ready`, read the Work via `deps.store.works().get(&work_id).await?`.
2. If `work.attempt_count >= deps.config.max_work_attempts`, do NOT call `spawner.override_work(...)`. Instead:
   a. Read the Plan via `deps.store.plans().get(plan_id).await?`.
   b. Call `plan.transition(PlanStatus::Stalled, Role::Director)`.
   c. Persist via `deps.store.plans().update(plan, expected_updated_at).await`. On `StoreError::Stale`, re-read and retry once; on second Stale, log a `warn!` and proceed to NeedHelp anyway — the Plan is moving, the daemon will reconverge on next reconcile.
   d. Return `Err(DirectorError::NeedHelp(format!("retry budget exhausted on work {work_id} (attempt_count={} >= max_work_attempts={})", work.attempt_count, deps.config.max_work_attempts)))`.

The Director-layer cap is the soft, well-behaved exit: Plan transitions to Stalled, Director exits with NeedHelp, daemon stays up cleanly.

**Layer 3 — spawner-layer hard cap (defense-in-depth circuit breaker).** Inside `transition_and_persist_work`, alongside the increment, add:

```rust
const MAX_WORK_ATTEMPTS_HARD_CAP: u32 = 100;
if matches!(target, WorkStatus::Ready) && work.attempt_count >= MAX_WORK_ATTEMPTS_HARD_CAP {
    return Err(/* a typed error variant, e.g. WorkUpdateError::HardCapExceeded */);
}
```

This is the "this should never fire" backstop. Today, the only retry caller is the Director's Layer-2 cap, which fires at `max_work_attempts = 3` (default). The hard cap at 100 means: if a future bug, rogue agent, manual CLI intervention, or unforeseen retry path bypasses Layer 2 and pushes a Work to Ready 100 times, the persist gate refuses and surfaces an error. The constant is intentionally far above any plausible operator-tunable `max_work_attempts` — an operator legitimately raising the soft cap to 10 or 20 still has 80+ headroom before the hard cap could fire.

**Why not single-layer?** The v3/v4 mechanism per project memory had this exact two-layer design and was load-bearing for the doom-loop fix. The cost is one constant + one if-statement; the benefit is a tripwire when something has bypassed the Director-layer gate. The two layers are not redundant: Layer 2 is the well-behaved exit (Plan→Stalled, Director→NeedHelp); Layer 3 is the hard refusal that prevents runaway LLM spend or infinite cycling.

**Cap scope (Layer 2):** the soft cap fires only when `target == WorkStatus::Ready`. Other override targets (e.g., `Blocked`, `Failed`) don't increment retry semantics and aren't gated. This keeps the gate aligned with the prompt's "3 attempts to make this Work succeed" framing.

### Data Model

```rust
// crates/domain/src/plan.rs (extend the FSM table — the existing
// transitions/overrides/role list shown verbatim from line 26-45 with
// only the NEW arms added).
#[derive(Fsm, ...)]
#[fsm(
    role = crate::Role,
    terminal = [Complete, Superseded, Abandoned],   // Stalled is NOT in this list — it has an outgoing override (Stalled=>Active), so the FSM derive's validate.rs:19-20 forbids including it in terminal_states. is_terminal(Stalled) returns false.
    transitions(
        Draft   => Pending    by (Reactor),
        Draft   => Active     by (Reactor),
        Draft   => Superseded by (Reactor, Director),
        Draft   => Abandoned  by (Reactor, Director),
        Pending => Active     by (Reactor),
        Pending => Superseded by (Reactor, Director),
        Pending => Abandoned  by (Reactor, Director),
        Active  => Complete   by (Reactor, Decomposer),
        Active  => Stalled    by (Director),                        // NEW
        Active  => Superseded by (Reactor, Director),
        Active  => Abandoned  by (Reactor, Director),
    ),
    overrides(
        Active   => Draft  by (Director),
        Pending  => Draft  by (Director),
        Stalled  => Active by (Director),                           // NEW: operator triggers via Director-role CLI
    ),
)]
pub enum PlanStatus {
    Draft,
    Pending,
    Active,
    Complete,
    Stalled,        // NEW: non-terminal quiescent state
    Superseded,
    Abandoned,
}
```

```rust
// crates/agents/src/config.rs
pub struct DirectorConfig {
    // ... existing fields ...
    pub max_restarts: u32,
    pub max_requeries: u32,
    pub max_parse_failures: u32,
    pub max_work_attempts: u32,        // NEW
    // ...
}

impl Default for DirectorConfig {
    fn default() -> Self {
        Self {
            // ...
            max_restarts: 3,
            max_requeries: 3,
            max_parse_failures: 3,
            max_work_attempts: 3,      // NEW
            // ...
        }
    }
}
```

```rust
// crates/loopr/src/daemon/context.rs
pub struct DaemonContext<L: ...> {
    // ... existing fields ...
    pub director_tasks: Mutex<JoinSet<()>>,
    pub work_spawner_tasks: Mutex<JoinSet<()>>,    // NEW
    // ...
}
```

### API Design

**`DaemonSpawner` spawn pattern.** Today (`crates/loopr/src/daemon/context.rs`):

```rust
impl<L: ...> WorkSpawner for DaemonSpawner<L> {
    fn accept_bundle(&self, bundle_id: BundleId) {
        let ctx = Arc::clone(&self.0);
        tokio::spawn(async move {
            if ctx.shutting_down.load(Ordering::Relaxed) { return; }
            // ... mutate store, spawn integrator, etc.
        });
    }
}
```

After:

```rust
impl<L: ...> WorkSpawner for DaemonSpawner<L> {
    fn accept_bundle(&self, bundle_id: BundleId) {
        let ctx = Arc::clone(&self.0);
        let ctx_for_lock = Arc::clone(&self.0);
        // Spawn a tiny shim task to do the lock + spawn; the lock is async,
        // and WorkSpawner methods are sync, so we bounce through one
        // tokio::spawn to reach the .await.
        tokio::spawn(async move {
            let mut tasks = ctx_for_lock.work_spawner_tasks.lock().await;
            tasks.spawn(async move {
                if ctx.shutting_down.load(Ordering::Relaxed) { return; }
                // ... mutate store, spawn integrator, etc.
            });
        });
    }
    // override_work and assign_work mirror the same pattern.
}
```

The shim-task approach is the cleanest way to bridge the sync `WorkSpawner` trait with the async lock. Alternatives discussed below.

**Drain ordering and rationale.**

```rust
// crates/loopr/src/daemon.rs (replacing the current drain block)

// Drain rationale: tasks must be drained in REVERSE-spawn-chain order so
// that no pool is finalized while an upstream pool can still spawn into
// it. Spawn chain in the daemon today:
//
//   Implementer → spawns Reviewer
//   Reviewer    → spawns Integrator (on Verdict::Accept)
//   Director    → spawns into work_spawner_tasks (accept_bundle / override_work / assign_work)
//   work_spawner→ spawns Integrator (when accept_bundle commits)
//
// So the drain order is the reverse of that DAG's topological sort:
// drain implementer first (it has the most upstream sources), then the
// successive downstream pools, ending with integrator (which is
// downstream of everything).
//
// Why is this not the order shipped in v0.7.11? The shipped order
// (`implementer → reviewer → integrator → director`) predates
// `work_spawner_tasks` and relies on a defensive observation about
// tokio::JoinSet semantics: `JoinSet::spawn` during a concurrent
// `join_next` is NOT orphaned — the new task is picked up by the
// drain loop. So a Director firing accept_bundle during the integrator
// drain spawned directly into integrator_tasks, and the drain's
// in-progress join_next loop caught it. This worked, but it conflated
// two distinct spawn surfaces (Reviewer-originated integrators vs.
// Director-originated integrators) into one pool, and required a
// reader to derive the JoinSet semantics from first principles to be
// sure no work was lost. With work_spawner_tasks separating the
// Director's side effects, the spawn DAG becomes explicit and the
// drain order falls out of it directly.
//
// See docs/design/2026-05-09-director-phase-1-followups.md "Drain
// Ordering Rationale" for the full case.
drain_implementer_tasks(&ctx).await;
drain_reviewer_tasks(&ctx).await;
drain_director_tasks(&ctx).await;
drain_work_spawner_tasks(&ctx).await;     // NEW
drain_integrator_tasks(&ctx).await;
```

### Implementation Plan

#### Phase 1: `PlanStatus::Stalled` variant + FSM transitions

**Model:** sonnet

- Add `Stalled` to the `PlanStatus` enum in `crates/domain/src/plan.rs`. Do NOT add to the `terminal = [...]` list (Stalled has an outgoing override; FSM derive's `validate.rs:19-20` forbids terminal states from having outgoing edges).
- Extend the `#[fsm(...)]` table: `Active => Stalled by (Director)` in transitions; `Stalled => Active by (Director)` in overrides. (Director-only on both sides; operator triggers Stalled→Active via existing Director-role CLI override path.)
- **Exhaustive-match sweep.** The Rust compiler will reject every unguarded `match plan.status` after Stalled is added. Sweep callsites: `crates/loopr/src/daemon/context.rs:1091, 1107, 1108` (`is_terminal` consumers — Stalled returns `false`, so terminal-state summary stats do NOT fire on Stalled, which matches the intended semantics: Stalled is paused, not done). `crates/loopr/src/daemon/startup.rs:373` (`status == PlanStatus::Active` filter — already excludes Stalled for free; this is what closes the cold-boot loop). Any pretty-printer (`Display` impls, CLI summary renderers) gets an explicit "Stalled" arm.
- Add a unit test in `crates/domain/src/plan/tests.rs` (or sibling tests file) covering: `Active → Stalled` valid for Director; `Active → Stalled` rejected for Reactor/User; `Stalled → Active` via override only (transition rejected); `Stalled → Stalled` is `Transition::Unchanged`; `Stalled.is_terminal()` is `false`.
- Verify `otto ci` inside `crates/domain` passes.

#### Phase 2: `startup_reconcile_directors` integration tests

**Model:** sonnet

- New file: `crates/loopr/tests/director_reconcile.rs`.
- Test 1 (`cold_boot_respawns_director_for_active_plan`): write a Plan in `Active` status to a fresh `.loopr/taskstore/`; spin up a daemon test harness; assert `ctx.director_tasks` has at least one in-flight task within a short window; shut down cleanly.
- Test 2 (`cold_boot_skips_stalled_plan`): write a Plan in `Stalled` status; spin up a daemon test harness; assert `ctx.director_tasks` has zero in-flight tasks for that plan_id; shut down cleanly. This is the test that locks in the Phase 4 fix.
- Reuse the existing test harness pattern from `tests/stage_8_plan_to_tick.rs` and `tests/stage_9_director_plan_to_tick.rs`.

#### Phase 3: `work_spawner_tasks` JoinSet

**Model:** sonnet

- Add `pub work_spawner_tasks: Mutex<JoinSet<()>>` to `DaemonContext` in `crates/loopr/src/daemon/context.rs`. Initialize in `DaemonContext::new`.
- Update `DaemonContext::new`'s callers (`build_context` in `daemon.rs`, plus the three test fixtures: `transport/handler/tests.rs`, `transport/client/tests.rs`, `transport/server/tests.rs`) — but since the field is initialized inside `new()` itself (no constructor argument), no callsite changes needed beyond the field init.
- Refactor `DaemonSpawner::{accept_bundle, override_work, assign_work}` to spawn through the shim-task pattern shown in API Design above.
- Add `drain_work_spawner_tasks` mirroring the other drains (10s soft timeout + abort_all fallback).
- Update the drain block in `run_active_daemon` to the new order, with the drain-rationale comment shown in API Design above.
- Existing tests must continue passing (no behavioral change for the happy path).

#### Phase 4: `max_work_attempts` enforcement

**Model:** opus

The "opus" call here is because the enforcement path interacts with FSM transitions, store writes, and error propagation — getting the order wrong creates a partial-update window where the Plan is Stalled in memory but Active on disk, or vice versa.

**Layer 1 — increment site:**
- In `transition_and_persist_work` (`crates/loopr/src/daemon/context.rs`), on any successful FSM transition where the post-transition `work.status == WorkStatus::Ready`, increment `work.attempt_count` BEFORE the store write. Fires for both initial `Pending → Ready` and `Blocked → Ready` retries — 1-based counting (a Work that has run once has `attempt_count = 1`).
- Domain `Work::transition` already returns `Transition::Unchanged` when the FSM short-circuits, so the increment correctly skips no-op transitions.

**Layer 2 — Director-layer soft cap:**
- Add `max_work_attempts: u32` to `DirectorConfig` (default 3). Add to `Default` impl.
- Add a YAML round-trip test in `crates/agents/src/config/tests.rs` (or wherever the existing DirectorConfig tests live) covering the new field.
- In `run_director_inner` (`crates/agents/src/director.rs`), inside the `DirectorAction::OverrideWork { work_id, target_status, reason }` arm:
  1. After parsing `target_status` into `WorkStatus` via `parse_work_status`, check whether `target == WorkStatus::Ready`. If not, dispatch as today (no enforcement).
  2. If `target == WorkStatus::Ready`, read the Work via `deps.store.works().get(&work_id).await?`.
  3. If `work.attempt_count >= deps.config.max_work_attempts`, do NOT call `spawner.override_work(...)`. Instead:
     - Read the Plan via `deps.store.plans().get(plan_id).await?`.
     - Call `plan.transition(PlanStatus::Stalled, Role::Director)?`.
     - Persist via `deps.store.plans().update(plan.clone(), expected_updated_at).await`. On `StoreError::Stale`, re-read and retry once; on second Stale, log a `warn!` and continue to the `NeedHelp` return.
     - Return `Err(DirectorError::NeedHelp(format!("retry budget exhausted on work {work_id} (attempt_count={} >= max_work_attempts={})", work.attempt_count, deps.config.max_work_attempts)))`.
  4. Otherwise (cap not exhausted), dispatch through `spawner.override_work(...)` as today.
- Persist order matters: persist `Stalled` BEFORE returning `NeedHelp`. The cold-boot loop closes only if the Plan is Stalled when the daemon restarts.

**Layer 3 — spawner-layer hard cap:**
- Define `pub const MAX_WORK_ATTEMPTS_HARD_CAP: u32 = 100;` alongside other daemon constants in `crates/loopr/src/daemon.rs`.
- In `transition_and_persist_work`, after Layer-1's increment but before the store write: if `target == WorkStatus::Ready && work.attempt_count >= MAX_WORK_ATTEMPTS_HARD_CAP`, return a typed error (likely a new `WorkUpdateError::HardCapExceeded { work_id, count }` variant). The store write does NOT happen; the caller surfaces the error.
- This layer is "this should never fire" defense-in-depth. Today, Layer 2 stops things at default-3 with a clean Plan→Stalled. Layer 3 catches anything that bypasses Layer 2 and pushes a Work to Ready ≥100 times, refusing the persist and emitting a hard error in the run log.

**Tests:**
- Unit test in `agents` (Director-layer): target != Ready does not trip the soft cap; cap=0 trips on first Ready override; cap=5 with attempt_count=4 does not trip; cap-trip transitions Plan to Stalled before returning NeedHelp.
- Unit test in `loopr` (transition_and_persist_work): Pending→Ready increments to 1; Blocked→Ready increments by 1 each call; Unchanged FSM result skips increment; Layer-3 hard cap at attempt_count=100 returns `WorkUpdateError::HardCapExceeded`.
- Integration test in `loopr/tests/director_reconcile.rs` (extending Phase 2): Director receives an `OverrideWork { target_status: "Ready" }` against a Work pre-seeded with `attempt_count=99` and `max_work_attempts=3`; assert the Plan transitions to Stalled, the Director exits with NeedHelp, and a subsequent daemon restart does NOT respawn a Director for that Plan.

#### Phase 5: Documentation and rollout

**Model:** sonnet

- Inline the drain-ordering rationale comment in `crates/loopr/src/daemon.rs` at the existing drain block (the comment currently at lines ~691-695). Replace it with the long block shown in the API Design section above. The inline comment is the canonical short version — the design doc's "Drain Ordering Rationale" section is the long version, and the comment ends with a pointer to it. Two places, one source of truth.
- Update `crates/loopr/CLAUDE.md` "Instrumentation (daemon side)" section to mention the new `drain_work_spawner_tasks` step.
- Update the auto-memory file `~/.claude/projects/-home-saidler-repos-scottidler-loopr/memory/project-director-phase-1-shipped.md` "Known gaps" list to mark items 1, 2, 3 as closed (this file lives outside the repo but is consumed by future sessions; updating it is part of thorough rollout).
- Mark this design doc `Status: Implemented`.
- Bump version (`/bump`), push, install.

## Drain Ordering Rationale (canonical reference)

This section is the canonical answer for "why does the drain order look the way it does?" Every comment in `daemon.rs` that touches the drain order links back here.

**The spawn DAG.** In the daemon as of v0.7.11, the per-record pipeline is:

```
Implementer ──spawns──▶ Reviewer ──spawns(Accept)──▶ Integrator
                                                        ▲
                                                        │
                                                spawns(accept_bundle)
                                                        │
Director ──spawns──▶ work_spawner ─────────────────────┘
                          │
                          ├── spawns Integrator (on accept_bundle DB commit)
                          └── (override_work, assign_work — DB only)
```

Every directed edge above is a `tokio::spawn` of a task into one of the daemon's tracked `JoinSet`s.

**The drain invariant.** A `JoinSet`'s drain (the loop `while tasks.join_next().await.is_some() {}`) yields when the set is empty. **It does NOT prevent further `JoinSet::spawn` calls** — those spawns enqueue new tasks that the drain's `join_next` loop will pick up if it's still active. So a "drained" pool isn't sealed; it's just observably-empty-at-this-moment.

The invariant we actually want is: **no pool can receive a NEW spawn after its drain has returned.** This is what closes off "task X spawned a follow-up task into pool Y, but pool Y already drained and we're past `try_unwrap`, so the follow-up runs against a torn-down store."

Two ways to enforce that invariant:

1. **Drain in reverse-spawn-chain order.** Drain the most-upstream pool first; by the time we reach the most-downstream pool, no upstream task is still alive to spawn into it. This is the approach this doc adopts.
2. **Defensive shutdown_check at every spawned task's body entry.** Even if a task is spawned into an already-drained pool, it observes `shutting_down=true` and exits before touching state. This is how v0.7.11 stays correct under its current "wrong-looking" drain order.

v0.7.11 uses approach 2 implicitly. The drain order is `implementer → reviewer → integrator → director`, with the comment justifying it as "drain Director AFTER Integrator so a Director that fired accept_bundle (which spawns into integrator_tasks) lets that Integrator land before the Director itself winds down." This relies on:

- `JoinSet::spawn` during concurrent `join_next` is non-orphaning (the new task IS picked up).
- The shutdown_check at task body entry catches any spawn that lands after `shutting_down` is set but before the drain returns.

The combination is correct, but it's load-bearing on a tokio API contract that isn't obvious without reading the JoinSet docs, and the code reviewer has to verify both paths to be sure no work is lost. A future contributor refactoring `DaemonSpawner` could break either property without the test suite noticing.

**Architect Round 1 read on the v0.7.11 order:** flagged the comment as "logically inverted" — i.e., claimed the current order was a bug. **This is incorrect** (verified against `tokio::JoinSet`'s source-level semantics for concurrent `spawn`/`join_next`). The shipped order is defensively correct, just structurally muddled.

**This doc's order:** with `work_spawner_tasks` as a separate pool (Phase 3), the spawn DAG has a clear topology: `implementer → reviewer → director → work_spawner → integrator`. Reverse-spawn-chain drain falls out directly. Approach 1 supersedes approach 2 — the shutdown_check stays as belt-and-suspenders, but the drain order alone is now sufficient.

## Alternatives Considered

### Alternative 1: Stalled as a transient field instead of a PlanStatus variant

- **Description:** Add `operator_intervention_required: bool` to the Plan struct instead of a new FSM state.
- **Pros:** No FSM table changes; no migration of existing serialized Plans.
- **Cons:** Plan transitions become non-orthogonal to the recovery field — code that switches on `PlanStatus` can miss the "is_stalled" axis. Operator-facing CLIs (`loopr plan list`, `loopr show <plan>`) get a worse UX because the field is hidden until you query the full record. The FSM is the right home for "what state is this Plan in"; bypassing it for orthogonal flags is what tier-1 cleanup design doc explicitly argued against.
- **Why not chosen:** The FSM is the source of truth for record state. Adding a non-FSM "actually it's stalled though" axis creates two sources of truth.

### Alternative 2: Lifeguard generalization for `max_work_attempts`

- **Description:** Reuse the `Lifeguard` machinery (`max_repeat_action`, `max_parse_failures`) and add `max_attempts` as a third dimension.
- **Pros:** One mental model for "limit-and-escalate" instead of two.
- **Cons:** Lifeguard tracks intra-iteration state (this Director iteration's actions, this iteration's parse failures). `attempt_count` is a per-Work field that survives across iterations and across daemon restarts. Forcing the same struct to track both shapes adds complexity without payoff.
- **Why not chosen:** Different time scales. Lifeguard is per-Director-iteration; `max_work_attempts` is per-Work-lifetime. Different shape; different home.

### Alternative 3: Hard-fail the Plan on retry exhaustion (Stalled = `Abandoned`)

- **Description:** When the cap fires, transition Plan to `Abandoned` (already terminal) instead of introducing `Stalled`.
- **Pros:** No FSM changes; existing tooling already handles `Abandoned`.
- **Cons:** `Abandoned` is operator-initiated by definition — repurposing it for retry-budget exhaustion conflates two reasons a Plan is dead. Recovery from `Abandoned` requires re-creating the Plan; recovery from `Stalled` is a single override transition. Operator UX matters.
- **Why not chosen:** Different semantics, different recovery path. Worth the FSM addition.

### Alternative 4: Keep `WorkSpawner` detached spawns; rely entirely on shutdown_check

- **Description:** Don't add `work_spawner_tasks`. Trust the existing shutdown_check at task body entry to make detached spawns safe at shutdown.
- **Pros:** Less code; no JoinSet bookkeeping.
- **Cons:** A spawn that's enqueued but hasn't run its first poll yet at the moment `try_unwrap` runs holds an `Arc<DaemonContext>` clone that's INVISIBLE to the `Arc::strong_count` check the shutdown logic uses to decide whether `try_unwrap` will succeed. `harness::shutdown` warning paths flagged exactly this scenario in the Phase 1 known-gaps memo. Keeping the spawns detached means the bug is mitigated but not closed.
- **Why not chosen:** The `JoinSet` is what makes the count observable; the shutdown_check is belt-and-suspenders. Both together is the right level of paranoia for shutdown correctness.

## Technical Considerations

### Dependencies

No new crates. The work touches three existing crates (`domain`, `agents`, `loopr`) plus their tests.

### Performance

- Phase 1 (FSM extension): zero runtime cost. One additional enum variant.
- Phase 3 (`work_spawner_tasks`): one extra `Mutex<JoinSet>` lock per `WorkSpawner` invocation. WorkSpawner methods are called only by the Director; rate is bounded by Director iteration cadence (typically seconds, not microseconds). Lock contention is non-existent in practice.
- Phase 4 (`max_work_attempts`): one extra store read per `AssignWork(Ready)` action. Negligible.

### Security

No new attack surface. The Stalled state doesn't expose anything an Active Plan didn't already expose.

### Testing Strategy

Per-phase tests above. Convergence test (across all phases): an integration test that artificially exhausts attempt_count, asserts Plan transitions to Stalled, then simulates a daemon restart and asserts the Director is NOT respawned for that Plan. This is the single most important test in the doc — it's what proves Phase 4's enforcement path actually closes the cold-boot loop, with all of Phase 1, 2, 3 underneath it.

### Rollout Plan

One commit per phase. Phases must land in order (1 → 2 → 4 → 5 by hard dependency; 3 can land anywhere but conventionally between 2 and 4). After Phase 5 lands, bump to v0.7.15 and push tag.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Phase 4 enforcement runs but Phase 1's Stalled persist fails (store error), leaving Plan in Active and Director exited — cold-boot loop reappears | Medium | High | Phase 4's order: persist Stalled FIRST, then return NeedHelp. If persist fails, propagate the store error (NOT NeedHelp); the daemon's restart logic re-invokes the Director, which will hit the cap again, but at least the symptom (loop) surfaces as a store error in the run log instead of silent Active. Test exercises the persist-success path; persist-failure path is documented but not asserted (would require a fault-injecting Store stub) |
| `work_spawner_tasks` shim-task pattern (sync trait → async lock) leaks a tokio task on shutdown if the shim itself races with shutdown | Low | Low | The shim is a tiny await sequence (lock + spawn). If `shutdown_notify` fires between the shim's `tokio::spawn` and its `lock().await`, the inner spawn never happens — fine. If `shutting_down=true` is observed by the inner task's body, it exits before doing work — fine. The only loss path is a shim that has acquired the lock and called `spawn` but the new task hasn't run its body check yet at `try_unwrap` time. The new drain pool is what catches this case |
| Operator clears a Stalled Plan via override but the underlying Work is still Blocked with the same problem; Director respawns and immediately re-Stalls | Medium | Low | This is correct behavior — the operator should fix the Work first (override its state, fix the goal text, etc.) before clearing the Plan. We document this in CLAUDE.md and the design doc. A "Stalled Plan with no operator changes since last Stall" is fine to re-Stall |
| Adding `Stalled` breaks downstream consumers of `PlanStatus` (CLI summaries, telemetry rollups, FSM exhaustiveness matches) | Medium | Medium | The compiler catches non-exhaustive match arms (Rust requires every variant). Phase 1 includes a sweep of every `match plan.status` in the codebase; tests verify each match handles Stalled explicitly |
| Architect's Q5 catch (cold-boot loop) is mis-scoped — the actual failure mode is different from what we're guarding against | Low | High | The convergence test described in Testing Strategy proves the failure mode end-to-end. If it passes, Q5 is closed. If it fails, we re-engage the Architect |

## Open Questions

- [ ] Does the Director need a new event variant `director.give_up { reason }` to surface to operators in real time, or is the run-log `warn!` sufficient? Lean: log only for now; add an event when operator UX needs it.
- [ ] Should `loopr plan list` (the existing CLI) get a `--stalled` filter, or is the existing default-includes-everything behavior sufficient? Lean: defer until operator friction warrants.

## References

- `crates/loopr/src/daemon/startup.rs:362-401` — `startup_reconcile_directors` (current shipped impl)
- `crates/loopr/src/daemon.rs:680-695` — current drain block (the comment that triggered this design)
- `crates/loopr/src/daemon/context.rs` — `DaemonSpawner` impl and `DaemonContext` struct
- `crates/agents/src/director.rs:345-405` — `run_director` outer loop
- `crates/agents/src/director.rs:414-...` — `run_director_inner` action dispatch (where Phase 4 enforcement lands)
- `crates/agents/src/config.rs` — `DirectorConfig` (where `max_work_attempts` lands)
- `crates/domain/src/plan.rs:30-95` — Plan struct + FSM table (where `Stalled` lands)
- `docs/design/2026-05-08-director-phase-1.md` — the design this doc follows up on
- Project memory: `project-director-phase-1-shipped.md` — the source of the three follow-ups
- Architect pre-design consultation: this conversation, prior turns
