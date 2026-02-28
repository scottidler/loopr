# Design Document: Loopr v3 MVP5 — Coordinator Control Loop & Sequential Dependency Enforcement

**Author:** Scott Idler
**Date:** 2026-02-28
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

MVP5 fixes the five structural failures that prevent Loopr from building software with sequential dependencies. The Coordinator becomes a continuous phase-gated control loop (not a one-shot generator), worktrees start from the latest published Tick (not stale HEAD), work item dependencies are generated and enforced, and convergence/termination controls prevent runaway loops. These changes transform Loopr from "spray agents and hope" to "sequence work like a real dev team."

## Problem Statement

### Background

Loopr v3 MVPs 1–4 built a solid orchestration spine: FSMs, TaskStore persistence, daemon-mediated single-writer correctness, streaming LLM agents, deterministic Integrator, and a TUI. The first autonomous end-to-end run against a real repo (todo-app, 2026-02-28) proved the pipeline plumbing works — Bundles flow through proposal→triage→review→accept→merge→publish. One bundle (Cargo.toml initialization) successfully traversed the entire pipeline and was published as a Tick.

### Problem

The run also exposed five structural failures that prevent Loopr from building anything with sequential dependencies:

**1. Coordinator is one-shot, not continuous.** `determine_generation_level()` walks Plan→Spec→Phase→Work once, then returns `None`. After the initial generation burst, the Coordinator idles forever. It never monitors implementer progress, detects failures, retries blocked items, or advances phases. In the todo-app run, the Coordinator generated everything in one burst at 03:04:55, then had nothing to do for 4h53m while agents failed repeatedly.

**2. No sequential phase execution.** All 3 phases activated simultaneously. Work items for "Initialize project," "Define data model," and "Write integration tests" were all created and assigned at the same time. Phase 2 implementers couldn't succeed because Phase 1 code hadn't been merged yet.

**3. Worktrees always start from HEAD, not the latest Tick.** `worktree_mgr.create(key, "HEAD")` means every implementer starts from the bare scaffold, regardless of how many Ticks have been published. After the Cargo.toml bundle was merged, new implementers should have started from that integrated state — but they still saw the bare repo. A `refresh()` method exists but is never called.

**4. No dependency enforcement.** `Work.dependencies: Vec<String>` exists but is never populated (0 of 58 work items had dependencies) and never checked before assignment. The generation prompt mentions dependencies but doesn't provide existing work item IDs for the LLM to reference.

**5. No convergence or termination.** No max retries per work item, no phase timeouts, no goal completion detection. The system ran for 4h53m, spawning 804 implementer sessions (90.6% failure rate), creating 58 work items (86% Abandoned), with no mechanism to stop.

### Quantified Impact (todo-app run)

| Metric | Value |
|--------|-------|
| Runtime | 4h 53m |
| Agent sessions spawned | 1,078 total (804 implementer, 180 reviewer, 90 researcher) |
| Implementer success rate | 9.4% (21/201 unique sessions completed) |
| Reviewer success rate | 11.7% (7/60) |
| Work items created | 58 (50 Abandoned, 6 Blocked, 2 InProgress) |
| Duplicate work item titles | 8 titles repeated 2–10x each |
| Dependencies set | 0 |
| Bundles merged | 1 of 7 |
| Ticks published | 1 |
| Phases completed | 0 of 3 |

### Goals

1. Coordinator operates as a continuous phase-gated control loop that monitors, retries, and sequences work
2. Phases execute sequentially — Phase N+1 activates only after Phase N completes
3. Worktrees start from the latest published Tick so implementers see previously merged code
4. Work item dependencies are generated with concrete IDs and enforced before assignment
5. Duplicate work items are detected and prevented
6. Convergence controls terminate runaway loops and detect goal completion

### Non-Goals

- Changing the Implementer, Reviewer, or Integrator agent logic (their pipelines work)
- Changing the FSM transition rules (they're correct)
- Changing the TaskStore persistence model
- Cross-phase work item dependencies (within-phase only for MVP5)
- Dynamic re-planning mid-phase (Coordinator retries and escalates, doesn't redesign phases)
- Parallel phase execution (strictly sequential for MVP5; future work)

## Proposed Solution

### Overview

Six changes, ordered by dependency and impact:

1. **Worktree base from latest Tick** — implementers see merged code
2. **Coordinator FSM** — continuous control loop with 5 states
3. **Dependency-aware generation & scheduling** — deps generated with IDs, enforced on assign
4. **Duplicate detection** — prevent work item explosion
5. **Convergence & termination** — retry limits, timeouts, goal completion
6. **Failure feedback** — learnings from failures inform retry decisions

### Architecture

#### Coordinator FSM

Replace the current one-shot `determine_generation_level()` approach with a proper state machine:

```
                    ┌──────────────┐
                    │   PLANNING   │
                    │              │
                    │ Generate:    │
                    │  Plan        │
                    │  Spec        │
                    │  Phases      │
                    └──────┬───────┘
                           │ All phases created
                           ▼
                    ┌──────────────┐
              ┌────►│ACTIVATE_PHASE│
              │     │              │
              │     │ Pick next    │
              │     │ phase by     │
              │     │ order.       │
              │     │ Generate its │
              │     │ Works    │
              │     │ with dep IDs │
              │     └──────┬───────┘
              │            │ Works created
              │            ▼
              │     ┌──────────────┐
              │     │  EXECUTING   │◄──────────┐
              │     │              │            │
              │     │ Monitor WIs  │  Retry/    │
              │     │ Assign impls │  reassign  │
              │     │ Triage/accept│            │
              │     │ bundles      │            │
              │     └──────┬───────┘            │
              │            │ All WIs Done       │
              │            │ OR stuck           │
              │            ▼                    │
              │     ┌──────────────┐            │
              │     │ PHASE_GATE   │────────────┘
              │     │              │  Not all done
              │     │ Check: all   │  (retry/escalate)
              │     │ WIs in phase │
              │     │ are Done?    │
              │     └──────┬───────┘
              │            │ Yes
              │            ▼
              │     More phases? ──Yes──┘
              │            │
              │            No
              │            ▼
                    ┌──────────────┐
                    │GOAL_COMPLETE │
                    │              │
                    │ Mark goal    │
                    │ done. Stop.  │
                    └──────────────┘
```

**Key principle:** The Coordinator FSM does NOT replace the Ralph Wiggum Loop. The FSM state is *context* injected into the RWL — each iteration still calls the LLM with the current state summary, and the LLM decides which actions to take. The FSM constrains *what the Coordinator should focus on* and *when to advance*, while the LLM decides *how* within those constraints. State-specific prompt sections tell the LLM "you are in Executing state, focus on monitoring work items and assigning implementers."

**State behaviors:**

| State | What Coordinator Does Each Iteration | Exit Condition |
|-------|--------------------------------------|----------------|
| `Planning` | Generate Plan (if none), Spec (if none), Phases (if none). One hierarchy level per iteration (existing Gap #28 guard). Takes 3+ iterations minimum (Plan, then Spec, then Phases). Validation gates still apply for Plan/Spec/Phase Draft→Active. | Active Plan AND Active Spec AND all Phases Active |
| `ActivatePhase` | Find next Phase by `order` that hasn't been completed. Generate Works for it with dependency IDs referencing earlier WIs in same phase. Auto-transition WIs with acceptance_criteria to Ready. | All Works for current Phase exist and are Ready |
| `Executing` | Monitor WI statuses. Assign implementers to Ready WIs whose dependencies are all Done. Triage proposed Bundles. Accept reviewed Bundles. Transition Integrated WIs to Done. Detect Blocked/Failed WIs and retry (up to max_retries). Spawn reviewers for proposed Bundles. Create Learnings from failures. | All WIs in current Phase are terminal (Done, Abandoned, or NeedHelp) |
| `PhaseGate` | Check WI outcomes. If ALL WIs are Done → Phase succeeds, transition Phase to Complete. If ANY WI is NeedHelp → escalate (emit need_help event, human decides). If remaining WIs are Abandoned → Phase partially succeeded, transition to Complete with warning. Check for next Phase by order. | Unconditional → ActivatePhase (if more phases) or GoalComplete |
| `GoalComplete` | Mark CoordinatorGoal inactive. Emit completion event with summary (phases completed, WIs done, bundles merged). Return Done. | Terminal |

**Coordinator state persistence:**

```rust
/// New fields on CoordinatorGoal (or a new CoordinatorState record)
pub struct CoordinatorState {
    pub id: String,
    pub goal_id: String,
    pub fsm_state: CoordinatorFsmState,
    pub current_phase_id: Option<String>,
    pub work_attempts: HashMap<String, u32>,  // work_id → retry count
    pub created_at: i64,
    pub updated_at: i64,
}

pub enum CoordinatorFsmState {
    Planning,
    ActivatePhase,
    Executing,
    PhaseGate,
    GoalComplete,
}
```

This record is persisted in TaskStore so the Coordinator survives daemon restarts. On startup, it reads its state and resumes from where it left off.

**Relationship to existing code:**

| Existing Function | Fate | Reason |
|-------------------|------|--------|
| `determine_generation_level()` | **Reused** inside `handle_planning()` | Still useful for deciding Plan vs Spec vs Phase generation within Planning state |
| `build_generation_footer()` | **Reused** inside `handle_planning()` and `handle_activate_phase()` | Handles Draft validation retry logic, still needed |
| `build_state_summary()` | **Reused** in all states | Still the primary context builder for the LLM |
| `check_phase_completion()` | **Reused** inside `handle_phase_gate()` | Already checks if all WIs in a phase are Done |
| `find_phase_needing_works()` | **Replaced** by phase-order lookup in `handle_activate_phase()` | Current function finds any phase without WIs; new code finds the *next* phase by order |
| `run_coordinator_iteration()` | **Rewritten** as state-dispatch | Core loop becomes: read FSM state → dispatch to state handler → state handler calls LLM → persist state |
| `run_coordinator()` | **Modified** | Outer loop stays (sleep, cancellation check). Inner call changes from `run_coordinator_iteration()` to state-dispatched handler |

**Per-iteration flow in Executing state (the most complex state):**

```
handle_executing(stores, bridge, session, coord_state):
    1. Check phase timeout → if exceeded, transition to PhaseGate
    2. Check goal timeout → if exceeded, return Done
    3. Scan WIs in current phase:
       - Count by status: Ready, InProgress, InReview, Integrated, Done, Blocked, NeedHelp, Abandoned
       - If all terminal → transition to PhaseGate
    4. Build context with:
       - State summary (existing build_state_summary)
       - FSM state = "Executing"
       - Current phase title and WI status breakdown
       - Failure learnings from this phase
       - Active implementer/reviewer sessions
    5. Call LLM with context → get actions
    6. Execute actions (existing action dispatch), handling:
       - AssignAgent → may return DependencyNotMet (skip, try next WI)
       - TriageBundle → existing flow
       - AcceptBundle → existing flow
       - Transition(wi, Done) → for Integrated WIs
       - CreateLearning → existing flow
    7. For each WI that went Blocked/Failed this iteration:
       - Increment work_attempts[wi_id]
       - If attempts > max_retries → transition WI to NeedHelp
       - Else → transition WI back to Ready for retry
    7b. For each WI with dependencies on Abandoned/NeedHelp WIs:
       - Cascade: transition dependent WI to Abandoned (dep will never be Done)
       - Create Learning explaining the cascade
    8. Persist updated CoordinatorState
    9. Return Continue (active_interval) or Done (if all terminal)
```

#### Worktree Base Resolution

Current (executor.rs:83):
```rust
let worktree_path = worktree_mgr.create(key, "HEAD");
```

Changed to:
```rust
let base_ref = resolve_worktree_base(&stores).await;
let worktree_path = worktree_mgr.create(key, &base_ref);
```

Where:
```rust
/// Returns the integration SHA of the latest Published Tick,
/// or "HEAD" if no Ticks have been published yet.
async fn resolve_worktree_base(stores: &Stores) -> String {
    let ticks = stores.ticks.lock().await;
    ticks.values()
        .filter(|t| t.status == TickStatus::Published)
        .max_by_key(|t| t.number)
        .and_then(|t| t.integration_sha.clone())
        .unwrap_or_else(|| "HEAD".to_string())
}
```

This ensures every new implementer starts from the latest integrated codebase, not the stale initial HEAD.

#### Dependency-Aware Generation

**Problem:** The generation-work.pmt prompt says "declare dependencies" but doesn't give the LLM existing work item IDs to reference.

**Fix:** `build_work_prompt()` in generation.rs already receives `existing_works: &[Work]`. Enhance the prompt to include their IDs explicitly:

```
## Existing Works in This Phase
{% for wi in existing_works %}
- ID: {{ wi.id }} | Title: "{{ wi.title }}" | Status: {{ wi.status }}
{% endfor %}

When declaring `dependencies`, use the exact IDs above.
New Works you create will be assigned IDs by the system — reference
them by position (e.g., "depends on Work 1 from this batch") and the
system will resolve IDs after creation.
```

**Intra-batch dependencies:** When the LLM generates multiple Works in one response, it can't know their future IDs. Solution: the LLM declares positional dependencies using a `batch_index` convention (e.g., `"dependencies": ["batch:0"]` means "depends on the first Work created in this batch"). The executor maintains a `batch_created_ids: Vec<String>` across action executions within a single iteration. After creating each Work, it appends the new ID. When processing a dependency like `"batch:0"`, it resolves to `batch_created_ids[0]`. If resolution fails (invalid index), the dependency is dropped with a warning log — phase gating still provides cross-phase ordering as a fallback.

#### Dependency Enforcement on Assignment

Two enforcement points — belt and suspenders:

**1. Daemon handler (`handle_agent_start`)** — authoritative guard, protects all paths:

```rust
// In handlers.rs, handle_agent_start()
if agent_type == "implementer" {
    let wi = stores.works.lock().get(&work_id).cloned();
    if let Some(wi) = wi {
        for dep_id in &wi.dependencies {
            if let Some(dep) = stores.works.lock().get(dep_id) {
                if dep.status != WorkStatus::Done {
                    return Err(JsonRpcError {
                        code: -32011,
                        message: format!(
                            "Cannot assign '{}': dependency '{}' is {} (must be Done)",
                            wi.title, dep.title, dep.status
                        ),
                    });
                }
            }
        }
    }
}
```

**2. Executor action handler** — surfaces the error cleanly to the Coordinator:

```rust
AgentAction::AssignAgent { agent_type, target_id } => {
    // ... existing auto-transition logic ...
    match bridge.request("agent.start", params).await {
        Ok(result) => Ok(ActionResult::AgentSpawned { .. }),
        Err(e) if e.code == -32011 => Ok(ActionResult::DependencyNotMet {
            work_id: target_id,
            message: e.message,
        }),
        Err(e) => Err(e),
    }
}
```

The Coordinator receives `DependencyNotMet` and knows to skip this WI for now and work on unblocked items first. The Coordinator's `handle_executing()` state handler naturally prioritizes WIs whose dependencies are all Done.

#### Duplicate Detection

In the daemon's `handle_work_create()` handler (not the executor), so ALL creation paths are protected:

```rust
// In handlers.rs, handle_work_create()
fn handle_work_create(stores: &Stores, params: &Value) -> Result<Value> {
    let phase_id = params["phase_id"].as_str().unwrap();
    let title = params["title"].as_str().unwrap();

    // NEW: Check for existing Work with same title in same phase
    let existing = stores.works.lock();
    let duplicate = existing.values().find(|wi| {
        wi.phase_id == phase_id
            && wi.title.to_lowercase() == title.to_lowercase()
            && !matches!(wi.status, WorkStatus::Abandoned)
    });
    if let Some(dup) = duplicate {
        return Err(JsonRpcError {
            code: -32010,
            message: format!(
                "Duplicate work item '{}' already exists in phase {} with status {} (ID: {})",
                title, phase_id, dup.status, dup.id
            ),
        });
    }
    // ... existing creation logic
}
```

The executor's CreateWork action receives the error via bridge and surfaces it to the Coordinator as `ActionResult::DuplicateDetected`. The Coordinator can then decide to reuse the existing item or adjust its approach.

#### Convergence & Termination Controls

**New config knobs** (config.rs):

```rust
pub struct CoordinatorConfig {
    // Existing
    pub max_pool: usize,           // default 1
    pub idle_interval_secs: u64,   // default 30
    pub active_interval_secs: u64, // default 5

    // New
    pub max_work_retries: u32,  // default 3
    pub phase_timeout_secs: u64,     // default 3600 (1 hour)
    pub goal_timeout_secs: u64,      // default 14400 (4 hours)
}
```

**Retry tracking:** `CoordinatorState.work_attempts` maps work_id → attempt count. Incremented each time an implementer is assigned to a work item. When count exceeds `max_work_retries`, the Coordinator transitions the WI to NeedHelp and emits a Learning about the repeated failure.

**Phase timeout:** If the current phase has been in `Executing` state longer than `phase_timeout_secs`, the Coordinator transitions all non-Done WIs to Abandoned, the Phase to Cancelled, and emits `need_help`.

**Goal timeout:** If total runtime exceeds `goal_timeout_secs`, the Coordinator stops gracefully, marks the goal as timed out, and emits a summary of what was completed.

#### Failure Feedback

When an implementer session transitions to Failed:

```rust
// In the agent exit handler (already exists in executor.rs)
if session.status == AgentStatus::Failed {
    let learning = Learning {
        content: format!(
            "Implementer failed on '{}' after {} iterations. Error: {}. \
             Consider: splitting into smaller tasks, providing more context, \
             or checking dependency ordering.",
            work.title, session.iteration, session.error_message
        ),
        source: "system".to_string(),
        source_id: work.id.clone(),
        confidence: 0.7,
        scope: "phase".to_string(),
        ..Default::default()
    };
    stores.learnings.lock().await.insert(learning.id.clone(), learning);
}
```

The Coordinator's context builder already fetches learnings by scope — so these failure learnings will appear in subsequent iterations, informing retry decisions.

### Data Model

**New record: `CoordinatorState`**

```rust
pub struct CoordinatorState {
    pub id: String,
    pub goal_id: String,
    pub fsm_state: CoordinatorFsmState,
    pub current_phase_id: Option<String>,
    pub work_attempts: HashMap<String, u32>,
    pub phase_activated_at: Option<i64>,      // for phase timeout
    pub goal_started_at: i64,                 // for goal timeout
    pub phases_completed: Vec<String>,        // phase IDs in completion order
    pub created_at: i64,
    pub updated_at: i64,
}
```

Implements `Record` trait, persisted in `coordinator_state.jsonl`.

**Work changes:** None to the struct. The `dependencies` field already exists. The change is that it gets populated.

**Config changes:**

```yaml
# loopr.yml additions
coordinator:
  max_work_retries: 3
  phase_timeout_secs: 3600
  goal_timeout_secs: 14400
```

### API Design

**New IPC methods:**

| Method | Purpose |
|--------|---------|
| `coordinator.get_state` | Return current CoordinatorState (FSM state, current phase, retry counts) |
| `coordinator.reset_state` | Clear CoordinatorState (for manual restart from scratch) |

**Changed IPC behavior:**

| Method | Change |
|--------|--------|
| `agent.start` (implementer) | Now checks work item dependencies before spawning |
| `work.create` | Now checks for duplicates before creating |

**New ActionResult variants:**

```rust
pub enum ActionResult {
    // Existing variants...

    // New
    DependencyNotMet {
        work_id: String,
        blocked_by: String,
        blocked_by_status: WorkStatus,
    },
    DuplicateDetected {
        existing_id: String,
        existing_status: WorkStatus,
        title: String,
    },
    PhaseCompleted {
        phase_id: String,
        next_phase_id: Option<String>,
    },
    GoalCompleted {
        goal_id: String,
        phases_completed: usize,
        works_completed: usize,
    },
}
```

### Implementation Plan

#### Phase 1: Worktree Base Resolution

**Files:** `src/agents/executor.rs`, `src/worktree/manager.rs`

1. Add `resolve_worktree_base()` function that queries Stores for latest Published Tick SHA
2. Replace `"HEAD"` with resolved base in `run_agent_task()`
3. Add test: mock two Published Ticks, verify latest SHA is chosen
4. Add test: no Published Ticks → falls back to "HEAD"

#### Phase 2: Coordinator FSM & State Persistence

**Files:** `src/agents/coordinator.rs`, `src/domain/coordinator_state.rs` (new), `src/daemon/context.rs`, `src/daemon/handlers.rs`

1. Create `CoordinatorState` domain type with Record trait
2. Add `coordinator_state` collection to Stores and TaskStore initialization
3. Add `coordinator.get_state` and `coordinator.reset_state` IPC handlers
4. Rewrite `run_coordinator()` to use FSM states instead of `determine_generation_level()`
5. Each state has a dedicated handler: `handle_planning()`, `handle_activate_phase()`, `handle_executing()`, `handle_phase_gate()`, `handle_goal_complete()`
6. Persist CoordinatorState after each iteration
7. On daemon restart, resume from persisted state
8. Tests: FSM transitions (Planning→ActivatePhase→Executing→PhaseGate→ActivatePhase→...→GoalComplete)
9. Tests: crash recovery (resume from each state)

#### Phase 3: Dependency-Aware Generation

**Files:** `src/agents/generation.rs`, `prompts/generation-work.pmt`, `src/agents/executor.rs`

1. Enhance `build_work_prompt()` to format existing WI IDs in the prompt
2. Add intra-batch dependency resolution in CreateWork executor (positional → real IDs)
3. Add dependency check in AssignAgent executor (all deps Done?)
4. Add `DependencyNotMet` ActionResult variant
5. Update Coordinator `handle_executing()` to handle DependencyNotMet (skip, work on blocker first)
6. Tests: generate WIs with deps, verify deps reference valid IDs
7. Tests: AssignAgent blocked when dep not Done, succeeds when dep Done

#### Phase 4: Duplicate Detection & Convergence

**Files:** `src/agents/executor.rs`, `src/config.rs`, `src/agents/coordinator.rs`

1. Add duplicate check in CreateWork handler
2. Add `DuplicateDetected` ActionResult variant
3. Add `max_work_retries`, `phase_timeout_secs`, `goal_timeout_secs` to config
4. Add retry counting in CoordinatorState.work_attempts
5. Add timeout checks in `handle_executing()` and `handle_phase_gate()`
6. Add `GoalCompleted` and `PhaseCompleted` ActionResult variants
7. Tests: duplicate detection (same title+phase → rejected)
8. Tests: retry limit (3 failures → NeedHelp)
9. Tests: phase timeout
10. Tests: goal completion detection

#### Phase 5: Failure Feedback & Prompt Updates

**Files:** `src/agents/executor.rs`, `prompts/coordinator.pmt`, `prompts/generation-work.pmt`

1. Create Learning on implementer failure with structured failure information
2. Rewrite coordinator.pmt for phase-gated orchestration (state-aware prompt sections)
3. Update generation-work.pmt with explicit ID reference instructions
4. Add state-specific context sections to Coordinator prompt builder
5. Tests: Learning created on failure
6. Tests: Coordinator context includes failure learnings

## Alternatives Considered

### Alternative 1: Let the LLM Figure It Out (Smarter Prompts Only)

- **Description:** Keep the current one-shot generation, but enhance prompts to tell the Coordinator to sequence phases and track dependencies.
- **Pros:** No code changes to the control loop. Prompt-only fix.
- **Cons:** The todo-app run already had prompts that said "declare dependencies" and "prioritize dependencies" — the LLM ignored them. LLMs are unreliable at maintaining state across 500+ iterations. The Coordinator prompt is already 51 lines; making it longer won't fix the structural issue that `determine_generation_level()` returns None after initial generation.
- **Why not chosen:** The problem is structural, not prompt-quality. The code doesn't enforce phase gating or dependency checking regardless of what the LLM says.

### Alternative 2: Parallel Phases with Cross-Phase Dependencies

- **Description:** Allow all phases to be active simultaneously but enforce cross-phase dependencies (Phase 2 WI depends on Phase 1 WI).
- **Pros:** More flexible. Could enable more parallelism.
- **Cons:** Significantly more complex dependency resolution. Cross-phase deps create a global dependency graph that's harder to reason about. The Coordinator would need to track dependencies across phases, handle circular dependency detection across phases, and manage stale worktrees when early-phase changes invalidate late-phase work. Also, the todo-app failure wasn't "not enough parallelism" — it was "zero sequencing."
- **Why not chosen:** Sequential phases are simpler, correct, and sufficient for MVP5. Parallel phases can be added later as an optimization.

### Alternative 3: Implementer-to-Implementer Branching (Chain Worktrees)

- **Description:** Instead of all worktrees branching from main/Tick, have later implementers branch from earlier implementers' worktrees directly.
- **Pros:** No need to wait for Integrator merge. Faster feedback.
- **Cons:** Creates a tree of dependent branches that's hard to merge. If implementer A's branch is rejected, implementer B's branch (based on A) is invalidated. Conflict resolution becomes O(n^2). The Integrator's deterministic merge-to-main model breaks.
- **Why not chosen:** The Tick-based integration model is a core design principle. Worktrees from latest Tick achieves the same goal (implementers see merged code) without the branch-chaining complexity.

## Technical Considerations

### Dependencies

- **Internal:** TaskStore Record trait (for CoordinatorState), existing FSM infrastructure, existing IPC framework
- **External:** None new. All changes use existing dependencies (tokio, serde, taskstore).

### Performance

- **Coordinator iteration cost:** One additional HashMap lookup per iteration (CoordinatorState). Negligible.
- **Dependency check on AssignAgent:** O(d) where d is number of dependencies per work item (typically 0-3). Negligible.
- **Duplicate detection:** O(n) scan of work items in phase (typically 3-10). Negligible.
- **Worktree base resolution:** One scan of Ticks collection (typically 1-5). Negligible.

### Testing Strategy

Each phase includes unit tests for its changes. Key test scenarios:

1. **Coordinator FSM transitions:** Planning→ActivatePhase→Executing→PhaseGate→ActivatePhase→...→GoalComplete
2. **Phase gating:** Phase 2 WIs not generated until Phase 1 WIs all Done
3. **Worktree base:** New implementers use latest Published Tick SHA
4. **Dependency enforcement:** AssignAgent rejects when deps not met
5. **Duplicate detection:** Same title+phase → DuplicateDetected
6. **Retry limits:** 3 failures → NeedHelp transition
7. **Crash recovery:** Resume from each FSM state after daemon restart
8. **Goal completion:** All phases Done → GoalComplete → Coordinator exits

### Rollout Plan

1. Implement Phase 1 (worktree base) — can be deployed independently, immediate improvement
2. Implement Phases 2-3 (Coordinator FSM + dependency generation) — deploy together, they're coupled
3. Implement Phase 4 (duplicate detection + convergence) — deploy independently
4. Implement Phase 5 (failure feedback + prompts) — deploy independently
5. Re-run todo-app as validation

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| LLM still doesn't generate dependencies despite prompt changes | Medium | High | Coordinator treats empty deps as "no blockers" — still works, just no ordering within a phase. Phase gating still provides cross-phase ordering. |
| Phase timeout too aggressive, kills good work | Low | Medium | Default 1 hour is generous for a single phase. Configurable via `phase_timeout_secs`. |
| CoordinatorState persistence adds latency | Low | Low | Single JSONL append per iteration (~1ms). Already doing this for AgentSession. |
| Intra-batch dependency resolution is fragile | Medium | Medium | Fall back to "no dependencies" if resolution fails. Log a warning. Phase gating still provides ordering. |
| Worktree base from old Tick causes merge conflicts | Medium | Medium | This already happens with HEAD. Integrator's StalePolicy handles it. Auto-replay-and-verify can rebase. |
| Coordinator FSM state gets stuck | Low | High | Phase timeout and goal timeout as backstops. Manual `coordinator.reset_state` IPC for recovery. |
| Dependency on Abandoned WI creates permanent block | Medium | High | Coordinator detects "dep is Abandoned" and cascades: dependent WI also goes Abandoned or NeedHelp. Check in `handle_executing()` each iteration. |
| Phase with 0 Works passes PhaseGate trivially | Low | Medium | Guard in `handle_phase_gate()`: require at least 1 Done WI to consider a Phase complete. Otherwise transition back to ActivatePhase to retry generation. |
| LLM hallucinates dependency IDs that don't exist | Medium | Low | Dependency check at AssignAgent validates each dep ID exists. Unknown IDs logged as warning and skipped (treated as no-op dep). |
| Integrator lag: Bundle Accepted but not yet Merged | Low | Low | Integrator cycle is 10s. Coordinator Executing state naturally waits (WI is InReview, not yet Integrated). Phase timeout covers pathological cases. |

## Open Questions

- [ ] **Coordinator prompt per state vs monolithic?** Recommendation: per-state prompt sections injected into a shared system prompt. The system prompt stays the same, but the "Current State" context section changes based on FSM state. Planning state gets generation instructions; Executing state gets monitoring/assignment instructions. This keeps the prompt manageable while providing state-relevant focus. Implementation: coordinator.pmt stays as the system prompt; a new `build_state_context()` function generates state-specific user context.
- [ ] **Retry counts on CoordinatorState vs Work?** Recommendation: CoordinatorState. Retry counts are Coordinator-session-specific (a new goal attempt starts fresh). Putting them on Work would persist across goal restarts, which is wrong. The Work record stays clean; the CoordinatorState tracks operational metadata.
- [ ] **Phase timeout behavior?** Recommendation: Abandon non-Done WIs and transition Phase to a new `TimedOut` status (or use existing Cancelled). Emit `need_help` event. Do NOT skip to next Phase automatically — later phases likely depend on the timed-out phase's output. The human or a future enhancement decides whether to retry or restructure.
- [ ] **Integrator → Coordinator wake-up?** Recommendation: defer for MVP5. The Coordinator's 5s active_interval means it will notice a published Tick within 5s. Adding event-driven wake-up is an optimization for a future MVP. The existing timer-based approach is simple and sufficient.

## Expected Outcome: Todo-App Re-Run

After MVP5, re-running the todo-app goal should produce:

```
Phase 1: Project Setup (3 WIs, ~15 min)
  WI 1: Initialize Cargo project with dependencies  → Implemented, Merged, Done
  WI 2: Define Todo struct with serde               → depends on WI 1, Implemented, Merged, Done
  WI 3: Implement JSON file storage layer            → depends on WI 2, Implemented, Merged, Done
  Tick #1 published (contains WI 1 bundle)
  Tick #2 published (contains WI 2+3 bundles)

Phase 2: CLI Commands (3 WIs, ~20 min)
  WI 4: Implement add and list commands              → depends on WI 3, worktree from Tick #2, Done
  WI 5: Implement complete and delete commands        → depends on WI 4, Done
  WI 6: Implement clap CLI argument parsing           → depends on WI 4, Done
  Tick #3 published

Phase 3: Testing & Polish (2 WIs, ~15 min)
  WI 7: Write integration tests                      → depends on WI 5+6, worktree from Tick #3, Done
  WI 8: Polish error messages and add README          → depends on WI 7, Done
  Tick #4 published

Goal: Complete. 4 Ticks, 8 WIs, ~50 min.
```

Key behavioral differences from the broken run:
- **8 work items** (not 58)
- **0 duplicates** (not 50 Abandoned copies)
- **3 phases executed sequentially** (not all at once)
- **Dependencies enforced** (WI 4 waits for WI 3 to be Done)
- **Worktrees from latest Tick** (Phase 2 sees Phase 1's code)
- **Convergence** (goal completes and Coordinator stops)

## References

- [MVP1 Design Doc](2026-02-25-loopr-v3-mvp1.md) — orchestration spine
- [MVP2 Design Doc](2026-02-26-loopr-v3-mvp2.md) — TaskStore + Doc Validator
- [MVP3 Design Doc](2026-02-26-loopr-v3-mvp3.md) — Implementer + Reviewer agents
- [MVP4 Design Doc](2026-02-26-loopr-v3-mvp4.md) — multi-level RWL, Coordinator, Integrator
- [E2E Blockers](2026-02-27-loopr-v3-e2e-blockers.md) — pipeline integration fixes
- [Audit Fixes](2026-02-27-loopr-v3-audit-fixes.md) — 23 defects found and fixed
- [Vision: Architecture Conversation](../v3-chatgpt-loopr-architecture-conversation.md)
- [Vision: MVP & FSM Conversation](../v3-claude-loopr-mvp-and-fsm-conversation.md)
- [Vision: Pre-plan Conversation](../v3-preplan-conversation.md)
- [Comprehensive Evaluation](../v3-comprehensive-evaluation.md) — pre-run assessment
