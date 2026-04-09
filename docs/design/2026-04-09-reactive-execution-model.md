# Design Document: Reactive Execution Model

**Author:** Scott A. Idler
**Date:** 2026-04-09
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Replace the cursor-based coordinator FSM execution model (ActivatePhase/PhaseGate/current_phase_id) with a unified dependency model and reactive reconciliation loop. One construct (deps) at every level, one promotion rule, one reconciliation function. Records born Pending, promoted when preconditions are met. Status reflects reality.

## Problem Statement

### Background

The coordinator FSM was designed in the 2026-02-28-coordinator-sequencing.md design doc to solve the first live run's chaos (58 simultaneous Work items, zero dependencies, 90% failure rate). It introduced a 7-state FSM with a phase cursor that walks the hierarchy sequentially: ActivatePhase -> Executing -> PhaseGate -> ActivatePhase (repeat).

This was the right fix at the time. But the system has grown past the cursor model and it is now the source of bugs.

### Problem

The E2E python-api run (2026-04-08) exposed a race condition:

1. The decomposer creates ALL Work items upfront with status `Ready`
2. Workers start polling before the coordinator sets `current_phase_id`
3. With `current_phase_id = None`, the work queue filter evaluates `unwrap_or(true)` - ALL Ready Work from ALL Phases is eligible
4. Phase 2 test Work (wk-v0qsj) ran before Phase 1 implementation Work (wk-n3ofl) finished
5. The implementer hit 404 on routes that did not exist yet, and blocked

The root cause is not the race itself but the architecture that makes it possible:

- **Two ordering mechanisms that conflict**: implicit hierarchy ordering (Spec `order`, Phase `order`) and explicit `dependencies` on Work items. When the decomposer gets one right but not the other, the system breaks.
- **Status lies about reality**: a Work born `Ready` in Phase 3 is not actually ready when Phase 1 is running. The system relies on a runtime filter to paper over this.
- **Phase gating split across three components**: the coordinator FSM sets `current_phase_id`, the worker reads it from coordinator state each tick, and the work queue filters by it. A race in any of the three breaks gating.
- **A cursor walking a structure is the wrong abstraction**: every mature task scheduler (Airflow, Argo Workflows, Make, Kubernetes controllers) uses declarative deps + reactive promotion, not a cursor.

### Goals

- Single ordering mechanism (deps) at every level: Spec, Phase, Work
- Record status reflects executable reality (Pending until actually runnable)
- No race conditions possible between decomposition and execution
- Simpler coordinator FSM (5 states instead of 7)
- Simpler work queue (no phase filter parameter)
- Simpler worker (no coordinator state lookup)
- Complete migration with no vestiges of the old model

### Non-Goals

- Parallel Spec execution (future work - but the architecture enables it naturally)
- DAG-shaped dependencies at Spec/Phase level (linked-list for now, DAG is a prompt-only change later)
- Changes to the Implementer, Reviewer, Integrator, or Researcher agents
- Changes to the Bundle, Tick, Lock, or Learning domain models
- Changes to the IPC protocol (handlers adapt, protocol unchanged)

## Proposed Solution

### Overview

Replace two ordering mechanisms (order field + deps) with one (deps only). Replace the cursor-based execution model (ActivatePhase/PhaseGate cycle) with a reactive reconciliation loop. Replace born-Ready status with born-Pending + promotion.

Three design rules:

1. **Deps are same-level, same-parent only.** Specs depend on sibling Specs (same Plan). Phases depend on sibling Phases (same Spec). Works depend on sibling Works (same Phase). No cross-level deps. No cross-parent deps (Phase in Spec A cannot depend on Phase in Spec B).
2. **Parent gate.** A child cannot activate until its parent is Active. Deps handle ordering within a level; parent status handles the cross-level gate.
3. **Status reflects reality.** A record is Pending until it can actually execute, then Ready/Active.

#### Concrete Example

Given a python-api project decomposition:

Deps shown as titles for readability; actual deps are resolved IDs (e.g. "sp-a7b1e").

```
Plan: Python Bookmarks API (Active - root is always Active)
  Spec A: Database Layer (Pending, deps=[])
    Phase 1: DB Module (Pending, deps=[])
      Work: database.py (Pending, deps=[])
      Work: test_database.py (Pending, deps=[])
    Phase 2: DB Validation (Pending, deps=["Phase 1: DB Module"])
      Work: integration_test.py (Pending, deps=[])
  Spec B: API Routes (Pending, deps=["Spec A: Database Layer"])
    Phase 1: App Module (Pending, deps=[])
      Work: main.py (Pending, deps=[])
      Work: requirements.txt (Pending, deps=[])
    Phase 2: API Tests (Pending, deps=["Phase 1: App Module"])
      Work: test_api.py (Pending, deps=[])
```

Reconciliation tick 1 (immediately after decomposition):
- Pass 1: Spec A has no deps -> promote to Active. Spec B has dep on Spec A (not Complete) -> stays Pending.
- Pass 2: Phase 1 of Spec A has parent Active + no deps -> promote to Active. Phase 2 of Spec A has dep on Phase 1 (not Complete) -> stays Pending. Spec B phases: parent Pending -> stays Pending.
- Pass 3: database.py and test_database.py have parent Phase Active + no deps -> promote to Ready.
- Workers pull database.py and test_database.py in parallel.

Reconciliation tick N (after both Works in Phase 1 of Spec A complete):
- Fixed-point iteration 1:
  - Passes 1-3 (Promotion): Phase 2 still Pending (Phase 1 not yet marked Complete).
  - Pass 5 (Completion): Phase 1 all Works terminal -> mark Complete. (completed=1)
  - State changed -> loop continues.
- Fixed-point iteration 2:
  - Pass 2: Phase 2 dep on Phase 1 (now Complete) + parent Active -> promote Active. (promoted=1)
  - Pass 3: integration_test.py parent Active + no deps -> promote Ready. (promoted=2)
  - Pass 5-6: no new completions.
  - State changed -> loop continues.
- Fixed-point iteration 3: no changes -> converged.
- Result: completion + promotion cascade in one reconcile() call.

Reconciliation tick M (after Phase 2 of Spec A completes):
- Fixed-point iteration 1:
  - Pass 5: Phase 2 Complete. Pass 6: All Spec A phases terminal -> Spec A Complete.
- Fixed-point iteration 2:
  - Pass 1: Spec B dep on Spec A (Complete) -> promote Active.
  - Pass 2: Phase 1 of Spec B parent Active + no deps -> promote Active.
  - Pass 3: main.py and requirements.txt promote Ready.
- Fixed-point iteration 3: no changes -> converged.
- Result: full three-level cascade in one reconcile() call.

No cursor. No current_phase_id. Just deps and parent gates.

#### What the Coordinator LLM Still Does

The reconciliation loop handles promotion and completion detection. But the coordinator LLM still runs each tick for coordination decisions:

- Monitoring Work status and diagnosing stuck/failing Works
- Tracking retry counts and SLA timeouts per Work (`work_attempts`, `work_first_assigned_at`)
- Triggering NeedHelp when max retries or abandon ratios exceeded
- Building LLM context via `build_execution_status()` (shows all Active Phases and their Works)
- Responding to the coordinator LLM's actions (AssignAgent, AbandonWork, etc.)

The reconciliation loop runs before the LLM call each tick. The LLM sees the post-reconciliation state.

### Architecture

#### Current Execution Flow

```
Decomposer creates all records (Specs=Active, Phases=Active, Works=Ready)
  |
Coordinator FSM: Planning -> ActivatePhase -> Executing -> PhaseGate -> ActivatePhase -> ... -> GoalComplete
  |                              |                |
  |                     sets current_phase_id    checks phase timeout, all works terminal
  |
Worker reads current_phase_id from CoordinatorState each tick
  |
Work queue filters Ready works by current_phase_id (unwrap_or(true) when None)
```

#### Proposed Execution Flow

```
Decomposer creates all records (Specs=Pending, Phases=Pending, Works=Pending)
  |
Coordinator FSM: Planning -> Executing -> GoalComplete
  |                              |
  |                     runs reconcile() each tick
  |
Reconciliation loop (3 passes):
  1. Promote Specs:  Pending -> Active when all spec-deps Complete
  2. Promote Phases: Pending -> Active when parent Spec Active AND all phase-deps Complete
  3. Promote Works:  Pending -> Ready  when parent Phase Active AND all work-deps Done
  Bottom-up completion detection:
  4. Phase -> Complete when all child Works terminal
  5. Spec -> Complete when all child Phases terminal
  6. Plan -> Complete (GoalComplete) when all Specs terminal
  |
Worker pulls any Ready Work (no phase filter, no coordinator state lookup)
```

#### Coordinator FSM

```
Before: Interviewing -> Decomposing -> Planning -> ActivatePhase <-> Executing <-> PhaseGate -> GoalComplete
After:  Interviewing -> Decomposing -> Planning -> Executing -> GoalComplete
```

- `ActivatePhase` and `PhaseGate` removed
- `Executing` now runs the reconciliation loop each tick
- Planning transitions directly to Executing when hierarchy exists
- Executing transitions to GoalComplete when reconciliation detects all Specs terminal, or on goal timeout
- `check_abandon_gate` still runs at GoalComplete - if too many records were Abandoned, it fires NeedHelp instead of completing

#### Reconciliation Function

```rust
fn reconcile(stores: &Stores, config: &CoordinatorConfig) -> ReconcileOutcome {
    let mut total_promoted = 0u32;
    let mut total_completed = 0u32;

    // Pre-pass: sweep Integrated Works to Done (carried over from current coordinator)
    sweep_integrated_to_done(stores);

    // Fixed-point loop: repeat promotion + completion passes until no state changes.
    // This guarantees a full multi-level cascade (Phase completes -> next Phase activates ->
    // Works promote to Ready) within a single reconcile() call, regardless of hierarchy depth.
    // Bounded by hierarchy depth (max iterations = depth of tree, typically 3-4).
    loop {
        let mut promoted = 0u32;
        let mut completed = 0u32;

        // --- Promotion passes (top-down) ---

        // Pass 1: Promote Specs (Pending -> Active when all spec-deps terminal)
        let spec_ids = collect_pending_spec_ids(stores);
        for spec_id in spec_ids {
            let (deps, eligible) = {
                let specs = stores.read_specs()?;
                let spec = specs.get(&spec_id)?;
                (spec.dependencies.clone(), spec.status() == HierarchyStatus::Pending)
            };
            if eligible && all_deps_terminal(stores, &deps, RecordLevel::Spec) {
                promote_spec(stores, &spec_id, HierarchyStatus::Active);
                promoted += 1;
            }
        }

        // Pass 2: Promote Phases (Pending -> Active when parent Active + phase-deps terminal)
        let phase_ids = collect_pending_phase_ids(stores);
        for phase_id in phase_ids {
            let (parent_id, deps, eligible) = {
                let phases = stores.read_phases()?;
                let phase = phases.get(&phase_id)?;
                (phase.parent_id.clone(), phase.dependencies.clone(), phase.status() == HierarchyStatus::Pending)
            };
            if eligible
                && parent_active(stores, &parent_id)
                && all_deps_terminal(stores, &deps, RecordLevel::Phase)
            {
                promote_phase(stores, &phase_id, HierarchyStatus::Active);
                set_phase_activated_at(stores, &phase_id, now_millis());
                promoted += 1;
            }
        }

        // Pass 3: Promote Works (Pending -> Ready when parent Active + work-deps Done)
        // Note: Work deps use all_deps_done (not terminal). See "Dependency Semantics" below.
        let work_ids = collect_pending_work_ids(stores);
        for work_id in work_ids {
            let (parent_id, deps, eligible) = {
                let works = stores.read_works()?;
                let work = works.get(&work_id)?;
                (work.parent_id.clone(), work.dependencies.clone(), work.status() == WorkStatus::Pending)
            };
            if eligible
                && parent_active(stores, &parent_id)
                && all_deps_done(stores, &deps)
            {
                promote_work(stores, &work_id, WorkStatus::Ready);
                promoted += 1;
            }
        }

        // --- Timeout pass ---

        // Pass 4: Phase timeout check
        for (phase_id, activated_at) in active_phases_with_activation(stores) {
            if now_millis() - activated_at > config.phase_timeout_secs as i64 * 1000 {
                abandon_non_terminal_works(stores, &phase_id);
                mark_phase_complete(stores, &phase_id);
                completed += 1;
            }
        }

        // --- Completion passes (bottom-up) ---

        // Pass 5: Phase completion (all child Works terminal -> Phase Complete)
        for phase_id in active_phase_ids(stores) {
            if all_children_terminal(stores, &phase_id, RecordLevel::Work) {
                mark_phase_complete(stores, &phase_id);
                completed += 1;
            }
        }

        // Pass 6: Spec completion (all child Phases terminal -> Spec Complete)
        for spec_id in active_spec_ids(stores) {
            if all_children_terminal(stores, &spec_id, RecordLevel::Phase) {
                mark_spec_complete(stores, &spec_id);
                completed += 1;
            }
        }

        total_promoted += promoted;
        total_completed += completed;

        // Fixed-point: if nothing changed this iteration, we are converged.
        if promoted == 0 && completed == 0 {
            break;
        }
    }

    // Goal complete? Check based on Plan tier.
    let goal_complete = detect_goal_complete(stores);

    ReconcileOutcome { promoted: total_promoted, completed: total_completed, goal_complete }
}

/// parent_active checks the parent record's status regardless of type.
/// Uses ID prefix to determine which store to check:
/// - "ph-" prefix -> Phase store
/// - "pl-" prefix -> Plan store (Brief mode)
fn parent_active(stores: &Stores, parent_id: &str) -> bool {
    if parent_id.starts_with("ph-") {
        stores.read_phases().ok()
            .and_then(|p| p.get(parent_id).map(|ph| ph.status() == HierarchyStatus::Active))
            .unwrap_or(false)
    } else if parent_id.starts_with("pl-") {
        stores.read_plans().ok()
            .and_then(|p| p.get(parent_id).map(|pl| pl.status() == HierarchyStatus::Active))
            .unwrap_or(false)
    } else {
        false
    }
}

/// detect_goal_complete handles both Full and Brief modes.
fn detect_goal_complete(stores: &Stores) -> bool {
    let Some(plan) = find_active_plan(stores) else { return false };
    if plan.tier == Tier::Brief {
        // Brief: all Works parented to Plan are terminal
        find_works_for_parent(stores, &plan.id)
            .iter()
            .all(|w| w.status().is_terminal())
    } else {
        // Full: all Specs are terminal
        find_specs_for_plan(stores, &plan.id)
            .iter()
            .all(|s| s.status().is_terminal())
    }
}
```

This function is idempotent. Running it twice with no state changes produces the same result. If the daemon crashes and restarts, reconciliation reads actual state and converges.

**Key semantic decisions in reconciliation:**

- **Fixed-point convergence.** The promotion and completion passes run in a `loop` until no state changes occur (`promoted == 0 && completed == 0`). This guarantees multi-level cascades complete in a single `reconcile()` call. Example: Phase 1 Works complete -> Pass 5 marks Phase 1 Complete -> next loop iteration Pass 2 promotes Phase 2 to Active -> Pass 3 promotes Phase 2 Works to Ready. All in one call. Bounded by hierarchy depth (typically 3-4 iterations max).
- **Dependency semantics diverge by level.** Hierarchy deps (Spec, Phase) use `all_deps_terminal` - Complete OR Abandoned satisfies the dep. An Abandoned Phase should not block the entire Spec from advancing; the quality gate at GoalComplete (`check_abandon_gate`) handles overall abandon ratios. Work deps use `all_deps_done` - only Done (not Abandoned) satisfies. Work deps exist for same-file conflicts: if Work A writes `main.py` and is Abandoned, Work B (which also writes `main.py`) must not execute against a broken file state. Abandoned Work deps block downstream; the coordinator's retry/NeedHelp logic handles the stuck Work.
- **Empty parents complete immediately.** `all_children_terminal` returns true for a parent with zero children (vacuous truth). A Phase with no Works is immediately Complete. This matches the current system.

#### Brief Mode

Works are parented to the Plan (no Specs or Phases). The Plan is the root - it has no parent and no deps, so the decomposer creates it as `Active` (not Pending). This is the only record that starts Active.

Reconciliation handles Brief mode naturally via `parent_active()`. This function checks the parent record's status regardless of its type - if parent_id is a Plan, it checks the Plan store; if parent_id is a Phase, it checks the Phase store. Since the Plan is Active, all Pending Works with no deps promote to Ready immediately.

For completion detection, Brief mode is also handled uniformly. The goal_complete check at the bottom of reconciliation looks at the Plan's direct children: if all Works parented to the Plan are terminal, the goal is complete. No Specs or Phases to check.

### Data Model

#### HierarchyStatus (Plan, Spec, Phase)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, FlexibleEnum, Fsm)]
#[serde(rename_all = "lowercase")]
pub enum HierarchyStatus {
    #[transitions(Pending(Coordinator), Abandoned(Coordinator))]
    Draft,
    #[transitions(Active(Coordinator), Abandoned(Coordinator))]
    Pending,
    #[transitions(Complete(Coordinator), Abandoned(Coordinator))]
    Active,
    Complete,
    Abandoned,
}
```

New `Pending` variant between Draft and Active. Transition path: Draft -> Pending -> Active -> Complete.

#### WorkStatus

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, FlexibleEnum, Fsm)]
pub enum WorkStatus {
    #[transitions(Pending(Coordinator), Abandoned(Coordinator))]
    Draft,
    #[transitions(Ready(Coordinator), Abandoned(Coordinator))]
    Pending,
    #[transitions(
        InProgress(Coordinator),
        Blocked(Coordinator),
        Abandoned(Coordinator),
        Done(Coordinator)
    )]
    Ready,
    #[transitions(Blocked, InReview(Implementer), Abandoned(Coordinator))]
    #[overrides(Ready(Coordinator), InReview(Coordinator))]
    InProgress,
    #[transitions(Ready(Coordinator), Abandoned(Coordinator))]
    Blocked,
    #[transitions(InProgress(Coordinator), Integrated(Integrator), Abandoned(Coordinator))]
    #[overrides(Ready(Coordinator))]
    InReview,
    #[transitions(Done(Coordinator, Integrator), Abandoned(Coordinator))]
    Integrated,
    Done,
    Abandoned,
}
```

New `Pending` variant between Draft and Ready. Transition path: Draft -> Pending -> Ready -> InProgress -> ...

#### Spec

```rust
pub struct Spec {
    pub id: String,
    pub parent_id: String,
    pub title: String,
    #[serde(default)]
    pub acceptance_criteria: AcceptanceCriteria,
    status: SpecStatus,
    #[serde(default)]
    pub dependencies: Vec<String>,  // NEW: replaces order
    pub created_at: i64,
    pub updated_at: i64,
}
// Remove: order: u32
// Remove: order param from Spec::new()
```

#### Phase

```rust
pub struct Phase {
    pub id: String,
    pub parent_id: String,
    pub title: String,
    status: PhaseStatus,
    #[serde(default)]
    pub dependencies: Vec<String>,  // NEW: replaces order
    #[serde(default)]
    pub activated_at: Option<i64>,  // NEW: set by reconciliation on Pending -> Active
    #[serde(default)]
    pub acceptance_criteria: AcceptanceCriteria,
    pub created_at: i64,
    pub updated_at: i64,
}
// Remove: order: u32
// Remove: order param from Phase::new()
```

#### CoordinatorFsmState

```rust
pub enum CoordinatorFsmState {
    Interviewing,
    Decomposing,
    Planning,
    Executing,      // reconciliation loop runs here
    GoalComplete,   // terminal
}
// Remove: ActivatePhase, PhaseGate
```

#### CoordinatorState

Remove fields:
- `current_phase_id: Option<String>`
- `phase_activated_at: Option<i64>`
- `phases_completed: Vec<String>`

Remove methods:
- `activate_phase(phase_id)`
- `complete_phase()`

### Decomposer Changes

#### records_to_hierarchy

- All records born `Pending` instead of `Active`/`Ready`
- Spec deps wired as linked list (second depends on first, third on second)
- Phase deps wired as linked list within each Spec
- Work deps: same-phase, same-file only (unchanged in principle)
- Remove `spec_counter`, `phase_counters` (no order field to assign)
- Remove call to `resolve_cross_branch_deps`

#### resolve_cross_branch_deps

Delete entirely. Cross-spec deps no longer exist. All deps resolve within their decomposition branch:
- Spec deps: resolved in Plan -> Spec pass (all specs produced together, titles in spec_map)
- Phase deps: resolved in Spec -> Phase pass (all phases for a spec produced together)
- Work deps: resolved in Phase -> Work pass (all works for a phase produced together)

#### Prompt template changes

**spec.pmt** - strengthen deps guidance:
```
Dependencies form a sequential chain.
- First Spec: dependencies = []
- Second Spec: dependencies = ["<first spec title>"]
- Third Spec: dependencies = ["<second spec title>"]
Dependencies are Spec titles only. Never reference Phase or Work titles.
```

**phase.pmt** - strengthen deps guidance:
```
Dependencies form a sequential chain within this Spec.
- First Phase: dependencies = []
- Second Phase: dependencies = ["<first phase title>"]
Dependencies are Phase titles within this Spec only. Never reference Phases from other Specs.
```

**work.pmt** - clarify scope:
```
Dependencies are same-phase Work titles only.
Most Work items should have NO dependencies (they run in parallel).
The ONLY valid reason for a dependency is a same-file conflict:
if two Works write the same file, they must be sequential.
Do NOT create dependencies for logical ordering (that is what Phases are for).
Do NOT reference Works from other Phases.
```

### Coordinator FSM Changes

#### check_fsm_transition

Remove `ActivatePhase` and `PhaseGate` match arms. Update:

- `Planning`: transition directly to `Executing` when hierarchy exists (Specs with Phases, or Works for Brief)
- `Executing`: run reconciliation, check goal timeout, transition to GoalComplete if `reconcile()` reports all Specs terminal or timeout exceeded

#### apply_fsm_transition

Remove all ActivatePhase/PhaseGate handling. GoalComplete path no longer calls complete_phase() or mark_phase_record_complete().

#### Remove functions

- `find_next_phase_to_activate()` - replaced by reconciliation
- `mark_phase_record_complete()` - replaced by reconciliation (detect_completions)
- `mark_spec_complete()` - replaced by reconciliation (detect_completions)
- `activate_spec_if_draft()` - replaced by reconciliation (Pending -> Active)

#### build_phase_status -> build_execution_status

Replace the single-phase view (keyed by current_phase_id) with a multi-phase view showing all Active Phases and their Works. The coordinator LLM needs to see what is currently executing to make decisions.

### Worker and Work Queue Changes

#### work_queue.rs

- Remove `current_phase_id` parameter from `next_assignable_work`
- Remove phase filter (`.filter(|w| current_phase_id.map(|pid| w.parent_id == pid).unwrap_or(true))`)
- Keep dep filter as defense-in-depth (reconciliation already checked, but belt-and-suspenders)
- Keep priority scoring, dedup filter, two-phase claim (all unchanged)

#### worker.rs

- Remove lines 42-53 (coordinator state lookup for current_phase_id)
- Call `next_assignable_work(&stores)` with no phase parameter

### IPC Handler Changes

#### handle_work_create

Currently promotes Draft -> Ready when acceptance_criteria is non-empty. Change to promote Draft -> Pending instead. Reconciliation handles Pending -> Ready.

```rust
// Before
if !acceptance_criteria.is_empty() {
    work.force_status(WorkStatus::Ready);
}

// After
if !acceptance_criteria.is_empty() {
    work.force_status(WorkStatus::Pending);
}
```

#### handle_spec_create, handle_phase_create

If these handlers set initial status, change to Pending instead of Active/Draft.

#### Cycle detection

Add cycle detection to Spec and Phase dependency validation in their create handlers, matching the existing `detect_dependency_cycle` pattern in handle_work_create.

### unblock_dependents (unchanged)

Keep this function in `src/daemon/handlers/work.rs`. It handles the `Blocked -> Ready` transition when a dependency completes. This is a different path from reconciliation's `Pending -> Ready`:

- `Pending -> Ready`: reconciliation loop (first-time promotion)
- `Blocked -> Ready`: event-driven (Work was InProgress, got blocked, blocker resolved)

### Timeouts

#### Goal timeout

Unchanged. `goal_started_at` on CoordinatorState. Checked in reconciliation. If exceeded, transition to GoalComplete.

#### Phase timeout

Move from single cursor (`phase_activated_at` on CoordinatorState) to per-Phase `activated_at` field. Set by reconciliation on Pending -> Active transition. Reconciliation checks each Active Phase's age against `phase_timeout_secs`. If exceeded, abandon non-terminal Works in that Phase, mark Phase Complete.

### TUI Display Ordering

With `order` removed, Specs and Phases need a deterministic display order. The decomposer produces records in a tight synchronous loop where `now_millis()` can return identical timestamps for adjacent records, making `created_at` sorting unstable.

Use **topological sort by deps**. For a linked list (A -> B -> C), this produces the natural execution order. The algorithm is simple: records with no deps sort first, then records whose deps have already been placed. For the linked-list pattern this is O(n) - just follow the dep chain from the head (the record with empty deps).

Implementation: a `topo_sort_by_deps(records)` utility function that takes a slice of records with `id` and `dependencies` fields and returns them in dependency order. Used by TUI views when displaying Specs within a Plan or Phases within a Spec.

### DocMarkdown Frontmatter

Spec and Phase frontmatter currently emits `order`. Change to emit `dependencies` (as a list of IDs). Phase additionally emits `activated-at`.

### Implementation Plan

#### Step 1: Domain model

Add `Pending` to HierarchyStatus and WorkStatus. Add `dependencies: Vec<String>` to Spec and Phase. Add `activated_at: Option<i64>` to Phase. Keep `order` temporarily (both exist during migration). Update `#[derive(Fsm)]` transition tables. Update FSM transition tests.

**Files:** `src/domain/plan.rs`, `src/domain/spec.rs`, `src/domain/phase.rs`, `src/domain/work.rs`, `loopr-derive/` (if Fsm derive needs update), `src/tests/fsm/`

**Gate:** `otto ci` passes.

#### Step 2: Decomposer

Wire linked-list deps at Spec and Phase levels. Change initial status to Pending for all records. Remove `resolve_cross_branch_deps`. Remove order counters from `records_to_hierarchy`. Update prompt templates.

**Files:** `src/decomposer.rs`, `prompts/decompose/spec.pmt`, `prompts/decompose/phase.pmt`, `prompts/decompose/work.pmt`

**Gate:** `otto ci` passes. Unit test: decomposed hierarchy has deps, all Pending, no order field used.

#### Step 3: Reconciliation loop

Implement `reconcile()` function. Wire into coordinator's Executing state. Add unit tests for promotion and completion detection.

**Files:** `src/agents/coordinator.rs` (new reconcile function or module), `src/agents/coordinator/run.rs`

**Gate:** `otto ci` passes. Unit tests: reconciliation promotes correctly, detects completions, handles Brief mode.

#### Step 4: Simplify coordinator FSM

Remove ActivatePhase and PhaseGate states. Remove `current_phase_id`, `phase_activated_at`, `phases_completed` from CoordinatorState. Remove `find_next_phase_to_activate`, `mark_phase_record_complete`, `mark_spec_complete`, `activate_spec_if_draft`, `complete_phase`, `activate_phase`. Update `check_fsm_transition`, `apply_fsm_transition`. Rewrite `build_phase_status` -> `build_execution_status`.

**Files:** `src/domain/coordinator_state.rs`, `src/agents/coordinator.rs`, `src/agents/coordinator/tests.rs`, `src/tests/integration/coordinator.rs`, `src/tests/integration/cycling.rs`, `src/tests/integration/fsm.rs`

**Gate:** `otto ci` passes.

#### Step 5: Simplify worker and work queue

Remove `current_phase_id` parameter from `next_assignable_work`. Remove coordinator state lookup from worker. Remove phase filter.

**Files:** `src/daemon/work_queue.rs`, `src/agents/worker.rs`, work queue tests

**Gate:** `otto ci` passes.

#### Step 6: Remove order field

Remove `order: u32` from Spec and Phase structs and constructors. Remove from handlers and DocMarkdown frontmatter. Update TUI views to use `topo_sort_by_deps()` for display ordering.

**Files:** `src/domain/spec.rs`, `src/domain/phase.rs`, `src/daemon/handlers/spec.rs`, `src/daemon/handlers/phase.rs`, `src/tui/`, `src/config.rs`

**Gate:** `otto ci` passes.

#### Step 7: Cleanup and E2E

Grep for any remaining references to removed concepts. Remove dead code. Run E2E python-api to validate full pipeline.

**Gate:** `otto ci` passes. E2E completes without phase ordering violations. No references to removed concepts in codebase.

## Alternatives Considered

### Alternative 1: Fix the race only (one-line fix)

- **Description:** Change `unwrap_or(true)` to `unwrap_or(false)` in the work queue phase filter. Workers with no `current_phase_id` pull nothing.
- **Pros:** Minimal change. Fixes the immediate bug.
- **Cons:** Does not fix the architectural problem. Two ordering mechanisms still conflict. Status still lies. Phase gating still split across three components.
- **Why not chosen:** Fixes a symptom, not the disease. The next E2E run will find a different manifestation of the same root cause.

### Alternative 2: Flat DAG (remove Specs and Phases entirely)

- **Description:** Flatten to Plan -> Work items only. All ordering via explicit dependency edges. Specs and Phases become labels, not execution boundaries.
- **Pros:** Maximally simple. One level, one ordering mechanism.
- **Cons:** Puts ALL dependency wiring burden on the LLM decomposer. The hierarchy gives structural defaults ("tests come after implementation") that the LLM gets for free with Phases. Without Phases, the LLM must enumerate every ordering edge, which is the weakest link.
- **Why not chosen:** The hierarchy earns its keep by providing implicit ordering via Phases. A flat DAG is simpler in theory but harder for the LLM to get right in practice.

### Alternative 3: Keep order field, add Pending status only

- **Description:** Keep `order` as the ordering mechanism. Add `Pending` status so Works are not born Ready. Fix the race but keep the cursor model.
- **Pros:** Smaller change. Fixes the immediate race condition.
- **Cons:** Still two ordering mechanisms. Still a cursor FSM. Still phase gating across three components. `order` and `deps` can still diverge.
- **Why not chosen:** Does not unify the model. Leaves the architectural complexity in place.

## Technical Considerations

### Dependencies

- **TaskStore** (`scottidler/taskstore`): no changes needed. Stores are generic over Record trait. New fields are serde-compatible.
- **loopr-derive**: `#[derive(Fsm)]` proc macro may need updating to support new `Pending` variant transitions. If the macro just reads `#[transitions(...)]` attributes, no macro changes needed - just add the attributes to the new variant.

### Performance

Reconciliation runs once per coordinator tick (5s active, 15s idle). Each pass iterates all records of one type. With typical hierarchy sizes (2-3 Specs, 2-5 Phases per Spec, 2-5 Works per Phase = 20-75 records total), this is negligible.

Event-driven wakeup on Work status changes (Done, Abandoned) triggers immediate reconciliation, so promotion latency is bounded by event propagation time, not poll interval.

### Testing Strategy

- **Unit tests for reconciliation**: test each promotion rule in isolation (Spec, Phase, Work), test completion detection, test Brief mode, test phase timeout, test idempotency
- **FSM transition tests**: update for 5-state FSM, add Pending variant transitions
- **Integration tests**: coordinator startup -> decomposition -> reconciliation -> workers pull -> completion
- **E2E test**: python-api run completes without phase ordering violations

### Rollout Plan

This is an internal architecture change. No external API, no user-facing config change. Deploy as a single version bump after all phases pass `otto ci`.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Decomposer LLM fails to wire linked-list deps correctly | Medium | High | Validate in decomposer: every non-first record at each level must have exactly one dep. Reject and re-decompose if violated. |
| Reconciliation tick latency delays promotion | Low | Medium | Event-driven wakeup on Done/Abandoned triggers immediate reconciliation. 5s active poll as fallback. |
| Brief mode regression | Low | High | Reconciliation handles Works parented to Plan naturally (parent Active, no deps = immediate promotion). Unit test for this case. |
| TaskStore JSONL migration for new fields | Low | Low | New fields have `#[serde(default)]` - existing records deserialize with empty deps and None activated_at. Removed `order` field is silently ignored by serde (no deny_unknown_fields). Existing Active records stay Active. No migration script needed. |
| Circular deps from LLM | Low | High | Add cycle detection to Spec and Phase create handlers, matching existing `detect_dependency_cycle` for Works. |
| Phase timeout behavior changes | Medium | Medium | Moved from single cursor to per-Phase activated_at. Functionally equivalent but now each Phase has its own clock. Test timeout behavior explicitly. |

## Open Questions

- [ ] Should the decomposer validate linked-list deps structurally (every non-first item has exactly one dep on the prior item) and reject/retry if violated?
- [ ] Should `reconcile()` live in a new module (`src/agents/coordinator/reconcile.rs`) or inline in coordinator.rs?

## References

- `docs/design/2026-02-28-coordinator-sequencing.md` - original phase gating design (superseded by this doc)
- `docs/design/2026-02-25-orchestration-spine.md` - daemon architecture (unchanged)
- `docs/design/2026-02-26-multi-level-rwl.md` - hierarchy and agent model (hierarchy unchanged, execution model updated)
- `docs/design/2026-03-01-implementer-completion-and-parallel-execution.md` - parallel work execution (deps model unchanged at Work level)
- E2E python-api run report (2026-04-08) - the bug that motivated this redesign
