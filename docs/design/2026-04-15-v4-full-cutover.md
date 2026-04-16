# Design Document: v4 Full Cutover - Kill All v3 Orchestration

**Author:** Scott A. Idler
**Date:** 2026-04-15
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

Delete every v3 orchestration agent (Coordinator, Integrator, Supervisor) and replace them with engine-driven YAML strategies wired through the existing composition engine. After this work, the engine is the ONLY orchestration path. No dual paths, no coexistence, no "partial cutover." The v3 agents are dead code and get deleted.

## Problem Statement

### Background

The v4 engine (composition engine, primitives, triggers, FSM interpreter) was built across Docs 2-6. The daemon's event loop already runs `engine.tick()` as the sole orchestration driver. 60 primitives are registered. 59 triggers are defined. Strategies exist for reconciliation, integration, recovery, supervision, sweeps, and decomposition.

But the old v3 agents are still alive:

- **Coordinator** (`src/agents/coordinator.rs`, 1,244 LOC + `coordinator/run.rs` ~800 LOC) - an LLM-driven FSM loop that makes orchestration decisions (assign agents, accept bundles, override works). Still spawned by `doc.rs` handler on plan acceptance.
- **Integrator** (`src/agents/integrator.rs`, 1,936 LOC) - a deterministic `run_cycle()` that merges bundles, creates ticks, handles conflicts. Still spawned via `agent.start` handler.
- **Supervisor** (`src/daemon/supervisor.rs`, ~200 LOC) - watches coordinator failures, restarts with backoff. Still runs alongside the engine.
- **Reviewer** (`src/agents/reviewer.rs`, ~300 LOC) - LLM bundle review with hallucination-prone sig extraction. Still spawned as an agent task.

The engine's strategies fire primitives that call `agent.start` via IPC bridge, which spawns these v3 agents as tokio tasks. Additionally, `auto_start_agents()` in `handlers.rs` is a post-dispatch hook that auto-starts implementers on work transitions and auto-triages bundles. This is v3 hardcoded behavior that should be engine strategies.

The engine wraps v3 - it doesn't replace it. This means:

- Bugs in v3 agents keep accumulating (last 4 tags: FSM races, action coherence, reviewer hallucinations)
- Every v3 fix is a band-aid on code the engine was supposed to replace
- The coordinator's LLM-driven decisions are unnecessary - all its actions are deterministic state transitions that the engine handles mechanically via triggers

### Problem

The v4 engine is the daemon's orchestration driver, but v3 agent code still runs inside it. We keep patching v3 agents that should be dead. ~25% of work since the vision doc has been v3 band-aids.

### Goals

- Delete `src/agents/coordinator.rs` and `src/agents/coordinator/` entirely
- Delete `src/daemon/supervisor.rs` entirely
- Delete `src/agents/integrator.rs` entirely
- Remove `AgentKind::Coordinator` and `AgentKind::Integrator` from the agent dispatch
- Add missing strategies that cover the coordinator's agent-lifecycle responsibilities
- Slim the reviewer to semantic-only review (no structural verification)
- Remove `CoordinatorGoal` and `CoordinatorState` from domain/stores
- `otto ci` passes with zero v3 orchestration code remaining
- E2E test passes with engine-only orchestration

### Non-Goals

- Rewriting the reviewer agent from scratch (it stays as an LLM agent, just scoped narrower)
- Adding primitives beyond what's needed for parity (except Director-related)
- Changing the TUI, CLI, IPC protocol, or TaskStore
- Hot-reloading strategies (still loaded at startup)
- Mechanical structural verification primitive (future work - separate design doc)

## Proposed Solution

### Overview

The v3 coordinator conflated two responsibilities:

1. **Mechanical scheduling** - spawn agents for ready works, triage bundles, handle review verdicts, promote hierarchy, detect goal completion. Every one of these is a deterministic state transition triggered by a condition. The engine handles this via triggers and strategies.

2. **Judgment and user interface** - interview the user to clarify plans, monitor execution with holistic context, diagnose failures ("is this work badly scoped or is the approach wrong?"), revise specs when implementation reveals they're impossible, escalate when mechanical recovery fails.

The engine replaces (1). A new **Director agent** (Opus-class) replaces (2). The Director is the top-level thinking agent - the "go-between" from the user to the system. It runs on Opus, activates on escalation or user interaction, and has authority to revise the plan hierarchy.

The integrator's job (merge bundles, validate, publish ticks) is entirely mechanical. The `integrate-tick` primitive replaces it.

The supervisor's restart logic is already covered by supervision strategies.

### The Director Agent

The Director is a new agent role that replaces the coordinator's JUDGMENT responsibilities. It does NOT run on every tick. It activates in three scenarios:

**1. Plan intake (interview flow):**
The user types a goal in chat. The Director interviews them (clarifying questions), shapes the input into a proper Plan with acceptance criteria, and submits it. Once the plan is accepted, the engine takes over mechanically (classify, decompose, promote, execute). The Director returns to idle.

**2. Escalation:**
When the engine's mechanical recovery fails (max retries exceeded, abandon ratio breached, SLA breached, conflict unresolvable), the `escalate` primitive activates the Director. The Director reads the full plan state, diagnoses the problem, and takes action:
- Rewrite a badly-scoped work item
- Revise or replace an un-implementable spec
- Split a too-large phase into smaller pieces
- Combine conflicting works with a resolution strategy
- Abandon a plan and explain why to the user

**3. User intervention:**
The user says something in chat during execution ("this isn't what I wanted", "focus on the API first", "skip the tests for now"). The Director interprets the intent and translates it into plan modifications.

**4. Environment/tooling failures:**
When implementers repeatedly fail due to environment issues (missing test runner, misconfigured build), the engine's retry strategies exhaust and escalate to the Director. The Director diagnoses whether the failure is code or environment, and either spawns a Researcher to discover the correct tooling, or surfaces the issue to the user ("I can't find a test runner for this repo - what should I use?").

**What the Director is NOT:**
- It is NOT a scheduler. It does not spawn agents, triage bundles, or promote hierarchy. The engine does that.
- It does NOT run on every tick. It runs on-demand (escalation event, user chat, plan intake).
- It does NOT replace the engine's mechanical loop. It supplements it with judgment for the cases the engine can't handle.

**Director role definition (YAML):**
```yaml
# strategies/roles/director.yml
name: director
model: claude-opus-4-6
max-tokens: 16384
temperature: 0.3
max-pool: 1
session-timeout-secs: 3600
prompt: director.pmt
tools:
  - read-file
  - grep
  - glob
  - shell
```

**Escalation strategy:**
```yaml
# Added to strategies/recovery.yml
escalate-to-director:
  trigger: escalation-needed
  scope: plan
  priority: 50
  action:
    - primitive: spawn-agent
      guard: no-active-director
      params:
        role: director
        target-id: $trigger.scope-id
```

The `escalation-needed` trigger is a composite: `abandon-ratio-exceeded OR work-sla-full-breach OR goal-timeout OR conflict-unresolvable`.

### Architecture

**Before (current):**
```
daemon.rs -> run_engine() -> engine.tick()
                              |
                              v
                         strategies fire primitives
                              |
                              v
                         primitives call agent.start IPC
                              |
                              v
                         handler spawns v3 agent task
                              |
                              v
                         v3 coordinator/integrator/reviewer runs
                         (LLM loop, hardcoded FSM, run_cycle)
```

Additionally, `auto_start_agents()` in `handlers.rs` fires as a post-dispatch
hook on every IPC response, auto-starting implementers on work transitions and
auto-triaging bundles. This is v3 hardcoded behavior outside the engine.

**After:**
```
daemon.rs -> run_engine() -> engine.tick()
                              |
                              v
                         triggers evaluate state
                              |
                              v
                         strategies fire primitives
                              |
                              v
                     +--------+--------+--------+--------+
                     |        |        |        |        |
                spawn-agent  transition  integrate-tick  complete  sweep
                (implementer, (bundle,    (merge, validate, (hierarchy) (cleanup)
                 reviewer,    work, plan)  publish tick)
                 decomposer)
```

**What stays as agent tasks (needs LLM judgment):**
- Implementer - writes code in worktrees
- Reviewer - semantic review of bundles
- Decomposer - breaks plans into specs/phases/works

**What gets deleted (never needed LLM, was always deterministic):**
- Coordinator - its "decisions" were always mechanical state transitions
- Integrator - git merge, validation, tick publish are mechanical operations
- Supervisor - restart logic is a recovery strategy

**What gets deleted from handlers:**
- `auto_start_agents()` post-dispatch hook - replaced by engine strategies

### Missing Strategies

These strategies must be added to close the gap between what the engine currently handles and what the coordinator handles:

**1. `spawn-implementer-for-ready-work`**
```yaml
# When a work item becomes Ready, spawn an implementer agent.
# This replaces the coordinator's AssignAgent action.
spawn-implementer-for-ready-work:
  trigger: work-ready
  scope: work
  priority: 800
  action:
    - primitive: spawn-agent
      guard: no-active-implementer-for-work
      params:
        role: implementer
        work-id: $trigger.scope-id
```
Requires new trigger: `work-ready` (state-query: work is Ready with no active implementer session).

**2. `auto-triage-proposed-bundle`**
```yaml
# When a bundle is proposed, auto-triage it to Triaged.
# The coordinator currently does this as an LLM action - but it's always
# a mechanical transition with no judgment involved.
auto-triage-proposed-bundle:
  trigger: bundle-proposed
  scope: bundle
  priority: 850
  action:
    - primitive: transition-record
      params:
        collection: bundle
        id: $trigger.scope-id
        target-status: triaged
        role: coordinator
```
Requires new trigger: `bundle-proposed` (event: `record-created` where collection=bundle).
Replaces: `auto_start_agents()` auto-triage hook in `handlers.rs` (lines 280-305).

**3. `spawn-reviewer-for-triaged-bundle`**
```yaml
# When a bundle is triaged, spawn a reviewer agent.
spawn-reviewer-for-triaged-bundle:
  trigger: bundle-triaged
  scope: bundle
  priority: 840
  action:
    - primitive: spawn-agent
      guard: no-active-reviewer-for-bundle
      params:
        role: reviewer
        bundle-id: $trigger.scope-id
```
Requires new trigger: `bundle-triaged` (state-query: bundle is Triaged with no active reviewer).

**4. `accept-approved-bundle`**
```yaml
# When a reviewer approves a bundle, transition it to Accepted.
accept-approved-bundle:
  trigger: reviewer-approved
  scope: bundle
  priority: 830
  action:
    - primitive: transition-record
      params:
        collection: bundle
        id: $trigger.scope-id
        target-status: accepted
        role: reviewer
```
Requires new trigger: `reviewer-approved` (event: reviewer session completed with verdict=Approve).

**5. `handle-rejected-bundle`**
```yaml
# When a reviewer rejects a bundle, transition it and retry the work.
handle-rejected-bundle:
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
    - primitive: retry-work
      params:
        work-id: $trigger.event.work-id
```
Requires new trigger: `reviewer-rejected` (event: reviewer session completed with verdict=Reject/RequestChanges).

**6-7. Hierarchy completion (ALREADY EXISTS)**
`complete-phases` and `complete-specs` already exist in `resources/engine/strategies/reconciliation.yml`. No new strategies needed. (Architect round 3 finding.)

**8. `complete-plan-on-goal`**
```yaml
# When all specs under a plan are terminal and goal is complete, complete the plan.
complete-plan-on-goal:
  trigger: goal-complete
  scope: plan
  priority: 850
  action:
    - primitive: transition-record
      params:
        collection: plan
        id: $trigger.scope-id
        target-status: complete
        role: coordinator
```

**9. `create-integration-branch-on-plan-active`**
```yaml
# When a plan becomes Active, create its integration branch.
# The coordinator currently does this when transitioning to Executing.
# Primitives: create-integration-branch already exists.
create-integration-branch-on-plan-active:
  trigger: plan-is-active
  scope: plan
  priority: 950
  action:
    - primitive: create-integration-branch
      params:
        plan-id: $trigger.scope-id
```

**10. `merge-integration-to-main-on-goal`**
```yaml
# When a plan's goal completes, merge the integration branch to main.
# Without this, code on integration/<plan-id> never reaches main.
# Must fire BEFORE complete-plan-on-goal (higher priority).
merge-integration-to-main-on-goal:
  trigger: goal-complete
  scope: plan
  priority: 860
  action:
    - primitive: merge-integration-to-main
      params:
        plan-id: $trigger.scope-id
  on-failure:
    - primitive: escalate
      params:
        reason: integration-merge-to-main-failed
```

**11. `delete-integration-branch-on-abandon`**
```yaml
# When a plan is abandoned, clean up its integration branch.
delete-integration-branch-on-abandon:
  trigger: plan-abandoned
  scope: plan
  priority: 800
  action:
    - primitive: delete-integration-branch
      params:
        plan-id: $trigger.scope-id
```
Requires new trigger: `plan-abandoned` (state-query or event: plan transitions to Abandoned).

**12. `resolve-structural-conflict`**
```yaml
# When integrate-tick detects a structural conflict, combine conflicting works.
resolve-structural-conflict:
  trigger: conflict-detected
  scope: work
  priority: 900
  action:
    - primitive: combine-conflicting-works
      params:
        work-ids: $trigger.event.conflicting-work-ids
```
Requires new trigger: `conflict-detected` (event emitted by `integrate-tick` when merge conflicts are structural).

**13. `revise-parent-on-impossible-spec`**
```yaml
# When a spec's works all fail (un-implementable), revise the parent by
# transitioning it back to Draft for re-decomposition.
# This is the upward feedback loop the coordinator handled via ReviseParent.
revise-parent-on-impossible-spec:
  trigger: spec-children-all-abandoned
  scope: spec
  priority: 870
  action:
    - primitive: transition-record
      params:
        collection: spec
        id: $trigger.scope-id
        target-status: draft
        role: coordinator
        reason: children-all-abandoned-bubble-up
```
Requires new trigger: `spec-children-all-abandoned` (state-query: all works under a spec are Abandoned).

**14. `register-tools-on-phase-active`**
```yaml
# When a phase becomes Active, register its validation commands as tools.
# The coordinator used to do this proactively. Without it, implementers
# can't run tests because the tools aren't registered in the daemon.
register-tools-on-phase-active:
  trigger: phase-is-active
  scope: phase
  priority: 890
  action:
    - primitive: register-validation-tools
      params:
        phase-id: $trigger.scope-id
```
Requires new primitive: `register-validation-tools` - reads phase's validation commands from config/hierarchy, registers them via `tools.register` IPC. Extracted from coordinator's tool registration logic.

**15. `spawn-researcher-for-missing-tools`**
```yaml
# When a phase is active but has no validation commands configured,
# spawn a researcher to discover test commands for the target repo.
# The coordinator used to do this via SpawnResearcher action.
spawn-researcher-for-missing-tools:
  trigger: phase-active-no-validation
  scope: phase
  priority: 880
  action:
    - primitive: spawn-agent
      guard: no-active-researcher-for-phase
      params:
        role: researcher
        target-id: $trigger.scope-id
        query: "discover test and validation commands for this phase"
```
Requires new trigger: `phase-active-no-validation` (state-query: phase is Active, has no validation commands registered).

**16. `handle-work-sla-breach`**
```yaml
# When a work item breaches its SLA, override it.
handle-work-sla-breach:
  trigger: work-sla-breach
  scope: work
  priority: 950
  action:
    - primitive: override-work
      params:
        work-id: $trigger.scope-id
        reason: sla-breach
```

### Implementation Plan

#### Phase 1: Director Agent and Missing Strategies
**Model:** opus (Director design) + sonnet (YAML wiring)

Add the Director agent role and the strategies that close the gap between what the engine handles and what the coordinator handled.

1. Add new triggers to `resources/engine/triggers/`:
   - `work-ready` - state-query: work is Ready, no active implementer session
   - `bundle-proposed` - event or state-query: bundle is Proposed
   - `bundle-triaged` - state-query: bundle is Triaged, no active reviewer session
   - `reviewer-approved` - event: reviewer completed with Approve verdict
   - `reviewer-rejected` - event: reviewer completed with Reject/RequestChanges verdict
   - `plan-abandoned` - state-query or event: plan transitions to Abandoned
   - `conflict-detected` - event: emitted by `integrate-tick` on structural conflict
   - `spec-children-all-abandoned` - state-query: all works under spec are Abandoned
   - `phase-active-no-validation` - state-query: phase is Active with no validation commands registered
2. Add new guard conditions to the `GuardConditionRegistry`:
   - `no-active-implementer-for-work` - checks no non-terminal implementer session exists for scope_id
   - `no-active-reviewer-for-bundle` - checks no non-terminal reviewer session exists for scope_id
   - `no-active-researcher-for-phase` - checks no non-terminal researcher session exists for scope_id
3. Add strategies to `resources/engine/strategies/`:
   - `agent-lifecycle.yml` - strategies 1-5 (spawn implementer, triage, spawn reviewer, accept/reject)
   - `completion.yml` - strategies 6-8 (complete phase, spec, plan)
   - `git-lifecycle.yml` - strategies 9-11 (create/merge/delete integration branch)
   - `conflict.yml` - strategy 12 (resolve structural conflicts)
   - `feedback.yml` - strategy 13 (revise parent on impossible spec)
   - `tooling.yml` - strategies 14-15 (register validation tools, spawn researcher for missing tools)
   - Add strategy 16 to existing `recovery.yml`
4. Add `register-validation-tools` primitive to `src/primitive/catalog/` - reads phase validation commands, calls `tools.register` IPC (extracted from coordinator's tool registration logic)
5. Ensure `integrate-tick` emits a `conflict-detected` event when it encounters structural merge conflicts (may require Rust change to the primitive)
5. Create Director agent:
   - Add `AgentKind::Director` to the agent kind enum
   - Add `resources/roles/director.yml` role config (Opus, max-pool 1, 1hr timeout)
   - Write `resources/agents/director.pmt` prompt:
     - Interview mode: clarify user goals, shape into Plan with AC
     - Escalation mode: diagnose failures, revise hierarchy, explain to user
     - Authority: can transition any record, revise specs, rewrite works
   - Add Director dispatch arm to `run_agent_loop` in `executor/lifecycle.rs`
   - Add `escalate-to-director` strategy to `recovery.yml`
   - Add `escalation-needed` composite trigger
   - Add `no-active-director` guard condition
6. Rewire `doc.accept` interview flow to use Director instead of Coordinator (the Director handles plan intake, the engine handles everything after)
7. Rewire chat escalation to spawn Director when user intervenes during execution
8. Validate: `otto ci` passes

#### Phase 2: Wire doc.accept to Director + Engine Path
**Model:** opus

The `doc.rs` handler currently creates a Plan, then starts a Coordinator agent. After this phase:
- If the plan needs interview/clarification: spawn a Director agent
- If the plan is fully formed (doc.accept with complete markdown): create Plan, emit `plan-approved`, engine takes over

1. In `src/daemon/handlers/doc.rs`:
   - Replace `agent.start coordinator` with `agent.start director` for the interview path
   - For the direct-accept path (doc.accept with full plan markdown): remove agent spawn entirely, just emit `plan-approved` event
   - Remove `coordinator_session_id` and `coordinator_already_running` from response
2. Verify: `plan-approved` event fires the `classify-and-configure` strategy, which fires decomposition, which cascades through the engine
3. Add integration test: submit a plan via `doc.accept`, verify decomposition happens without coordinator
4. `otto ci` passes

#### Phase 3: Remove auto_start_agents Hook
**Model:** sonnet

The `auto_start_agents()` post-dispatch hook in `handlers.rs` is v3 behavior that duplicates what engine strategies should do.

1. Delete `auto_start_agents()` function from `handlers.rs`
2. Remove the call site in `dispatch()` (lines 226-239)
3. The engine strategies from Phase 1 now handle:
   - Implementer spawning (previously: auto-start on work.transition to InProgress)
   - Bundle auto-triage (previously: auto-triage on bundle.create)
4. `otto ci` passes

#### Phase 4: Delete Coordinator Agent
**Model:** opus

The coordinator agent is now unreachable (no code path spawns it). Delete it.

1. Delete `src/agents/coordinator.rs` and `src/agents/coordinator/` directory
2. Remove `AgentKind::Coordinator` from `AgentKind` enum
   - BUT: keep `Role::Coordinator` in the FSM authorization system (strategies still use `role: coordinator` for transitions)
3. Remove `Coordinator` arm from `run_agent_loop` match in `executor/lifecycle.rs`
4. Remove coordinator config from `Config` struct (`config.agents.coordinator`)
5. Remove `CoordinatorGoal` and `CoordinatorState` from domain types and stores
6. Remove `coordinator_goals` and `coordinator_states` from `Stores` struct
7. Remove `src/daemon/handlers/coordinator.rs` handler file
8. Remove coordinator routes from handler dispatch
9. Clean up imports, dead code, stale references
10. `otto ci` passes

#### Phase 5: Delete Supervisor and Update Supervision Strategies
**Model:** sonnet

The supervisor module watches the coordinator. With the coordinator deleted, it's dead code. Supervision strategies already exist but reference "coordinator" - update them to reference "director".

1. Delete `src/daemon/supervisor.rs`
2. Remove supervisor references from daemon startup
3. Remove `SupervisorConfig` from config
4. Update `resources/engine/strategies/supervision.yml`:
   - `restart-coordinator-on-event` -> `restart-director-on-event`
   - `restart-coordinator-on-state` -> `restart-director-on-state`
   - Update trigger references (`coordinator-failed` -> `director-failed`, `no-running-coordinator` -> check appropriate)
5. Update corresponding triggers in `resources/engine/triggers/engine.yml`
6. `otto ci` passes

#### Phase 6: Delete Integrator Agent
**Model:** opus

The `integrate-tick` primitive currently delegates to `integrator.tick` IPC, which **does not exist** as a handler endpoint. Only `integrator.validate` and `integrator.publish` exist. The primitive is scaffolding that has never actually worked at runtime.

The integrator's `run_cycle()` (1,936 LOC) handles:
- Collecting accepted bundles for a plan
- Sequential git merge of bundle branches into integration branch
- Conflict detection and classification (structural vs retryable)
- Creating Tick records with validation commands
- Running validation (shell commands)
- Publishing ticks (transition to Published) or failing them
- Rejecting bundles and resetting works on failure
- Stale bundle detection
- Git state audit (branch existence, SHA reachability, merge ancestry)

This is the most complex primitive extraction. The strategy is: extract the core of `run_cycle()` into the `integrate-tick` primitive's `execute()` body directly (no IPC delegation), keeping the same logic but making it callable from the engine.

1. Rewrite `IntegrateTick::execute()` to contain the integration logic directly instead of delegating to a non-existent IPC endpoint. Extract from `IntegratorAgent::run_cycle()`:
   - Bundle collection query (from stores)
   - Sequential branch merge (git commands)
   - **Transition merged Works from InReview to Integrated** (without this, `sweep-integrated-to-done` never fires and plans never complete - Architect review finding)
   - **Emit `bundle.merged` event after each successful merge** (active implementers use this to rebase their worktree branches against integration tip - without it, they work on stale branches causing guaranteed conflicts - Architect review finding)
   - Tick creation, validation, publish/fail
   - Bundle rejection and work reset on failure
2. Extract `combine_conflicting_works()` into `CombineConflictingWorks` primitive (already registered but may need the implementation from `integrator.rs`)
3. Verify `AuditBranches`, `AuditTickShas`, `AuditMergeAncestry` primitives cover the integrator's audit logic
4. Delete `src/agents/integrator.rs`
5. Remove `AgentKind::Integrator` from enum
6. Remove integrator arm from `run_agent_loop`
7. Retain `IntegratorConfig` (validation commands, timeouts) - the engine still needs this config
8. Retain `handlers/integrator.rs` handler endpoints (`integrator.validate`, `integrator.publish`) as they're used by primitives
9. `otto ci` passes

#### Phase 7: Slim Reviewer to Semantic-Only
**Model:** opus

The reviewer stays as an LLM agent (semantic review needs LLM judgment). But remove the structural verification code that causes hallucinations.

1. Remove `extract_referenced_signatures()` from `reviewer.rs` - the function that reads file signatures and injects them as "ground truth" (the root cause of hallucination loops)
2. Remove `tentative` field from `Learning` and `is_structural_claim()` heuristic
3. Remove `disputed` field from `Bundle` and dispute detection from `handlers/bundle.rs`
4. Remove `code_citation` from `Learning` and `CodeCitation` struct
5. Remove arbitration path from reviewer (arbitrator prompt, dispute handling)
6. Delete `resources/agents/arbitrator.pmt`
7. Update reviewer prompt to scope review to semantic judgment only: "You are reviewing the logic and correctness of this implementation. Structural verification (signatures, interfaces, types) is handled mechanically. Focus on: does the logic make sense? Does it satisfy the acceptance criteria? Are there bugs?"
8. Remove retroactive tentative migration from `daemon/context.rs`
9. `otto ci` passes

#### Phase 8: Dead Code Cleanup
**Model:** sonnet

Final sweep for anything left behind.

1. Remove `AgentKind::Coordinator` and `AgentKind::Integrator` from all match arms, config, and references
2. Remove unused handler endpoints (coordinator.*, integrator.*)
3. Remove stale test files (coordinator tests, integrator tests, supervisor tests)
4. Delete dead `AgentAction` variants that were coordinator-only scheduling decisions now handled by engine strategies:
   - `AssignAgent` - replaced by `spawn-implementer-for-ready-work` strategy
   - `AcceptBundle` - replaced by `accept-approved-bundle` strategy
   - `ValidateDocument` - replaced by `validate-after-decomposition` strategy
   - `EvaluateCoverage` - replaced by `re-decompose-on-gaps` strategy
5. Re-attribute remaining coordinator actions to Director:
   - `CreateWork`, `OverrideWork`, `OverridePhase`, `OverrideSpec`, `ReviseParent` - Director judgment actions
   - `InterviewQuestion`, `ProposePlan` - Director intake actions
   - `SpawnResearcher` - shared between Director (manual) and engine `tooling.yml` strategy
6. Remove unused imports across all files
7. Run `cargo clippy` - zero warnings
8. Run `otto ci` - passes
9. Run E2E test - passes with engine-only orchestration

## Alternatives Considered

### Alternative 1: Keep Coordinator as a "Thin Scheduler"
- **Description:** Strip the coordinator's LLM loop but keep it as a deterministic scheduler that reads state and dispatches primitives.
- **Pros:** Familiar code path. Lower risk of missing edge cases.
- **Cons:** Still a separate process that duplicates what the engine does. Still has its own state (CoordinatorGoal, CoordinatorState). Still a dual path.
- **Why not chosen:** The engine IS a thin scheduler. Having two thin schedulers is the problem we're solving.

### Alternative 2: Incremental Migration (Keep Coordinator, Add Strategies)
- **Description:** Add engine strategies alongside the coordinator. Gradually disable coordinator actions as strategies take over.
- **Pros:** Lower risk per step.
- **Cons:** This is EXACTLY what we did. It produced the current mess. Both systems run, both can make conflicting decisions, bugs live at the boundary. The v4 vision doc explicitly rejected this as Alternative 3.
- **Why not chosen:** We tried this. It failed. 25% of work since the vision doc was patching v3 code that should be dead.

### Alternative 3: Kill Everything, Rewrite From Scratch
- **Description:** Delete all agent code. Rewrite implementer, reviewer, decomposer from engine-native primitives.
- **Pros:** Cleanest possible architecture.
- **Cons:** Massive scope. The implementer, reviewer, and decomposer are working LLM agents that genuinely need LLM loops. They're not the problem - the coordinator and integrator are.
- **Why not chosen:** Targeted deletion of coordinator/integrator/supervisor achieves the same goal without touching working agent code.

## Technical Considerations

### Dependencies

- **Internal:** No new crates. All primitives, triggers, and engine infrastructure already exist.
- **External:** No new dependencies.

### Performance

- The engine tick loop replaces the coordinator's LLM-driven decision loop. Engine ticks are microseconds (state inspection + HashMap lookups). Coordinator LLM calls were seconds per decision. This is a performance improvement.
- Integrator operations (git merge, validation) move from `IntegratorAgent::run_cycle()` to `integrate-tick` primitive. Same operations, same performance, different call site.

### Security

- No change. Agent sessions still use the same IPC bridge. Tool sandboxing unchanged.

### Testing Strategy

- **Phase 1:** `otto ci` validates YAML parsing and strategy cross-validation
- **Phases 2-5:** `otto ci` after each deletion. Compilation failures immediately reveal missed references.
- **Phase 7:** Full E2E test with engine-only orchestration
- **Regression:** The existing engine tests (1,705 LOC in `engine/tests.rs`) validate strategy execution
- **New tests:** Integration test in Phase 2 that submits a plan and verifies decomposition without coordinator

### Rollout Plan

- All work on v4 branch (current)
- Each phase is a separate commit
- `otto ci` gate between every phase
- No backward compatibility needed (clean break, v4 branch)

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Missing strategy for a coordinator edge case | Medium | Medium | Audit coordinator's `execute_action()` exhaustively before deletion. Any missed action shows up as a stuck work in E2E. |
| `integrate-tick` primitive doesn't cover all integrator behavior | Medium | High | Phase 5 includes explicit audit. Gaps get filled with new primitives extracted from integrator code, not hand-written. |
| Reviewer quality drops without sig extraction | Low | Medium | Sig extraction was causing MORE harm than good (hallucination loops). Removing it is a quality improvement. |
| Event ordering differences between engine-driven and coordinator-driven flow | Medium | Medium | Engine uses the same events. Level-triggered fallbacks (principle 8) catch anything the event path misses. |
| Stale triggers/strategies reference deleted code | Low | Low | Startup cross-validation catches all dangling references before any work starts. |

## Resolved Questions

- [x] **Integration branch lifecycle:** The primitives `CreateIntegrationBranch`, `MergeIntegrationToMain`, `DeleteIntegrationBranch` already exist. Added strategies 9-11 to wire them (Architect review finding).
- [x] **Conflict resolution:** `CombineConflictingWorks` primitive exists. Added strategy 12 and `conflict-detected` trigger (Architect review finding).
- [x] **Upward revision feedback:** Added strategy 13 (`revise-parent-on-impossible-spec`) to handle the bubble-up loop the coordinator did via `ReviseParent` (Architect review finding).
- [x] **`Role::Coordinator` after `AgentKind::Coordinator` deletion:** Yes, `Role::Coordinator` stays. It is a permission label in the FSM authorization system, not an agent type. The engine impersonates `Role::Coordinator` when executing strategies that need that authorization level. This is explicit and intentional - the engine IS the coordinator now.

## Resolved Questions

(continued)
- [x] **Work InReview to Integrated transition:** Must be included in `integrate-tick` primitive. Without it, `sweep-integrated-to-done` never fires (Architect review round 2).
- [x] **`bundle.merged` event emission:** Must be included in `integrate-tick` primitive. Active implementers need this to rebase branches (Architect review round 2).
- [x] **Tool registration:** Added `register-tools-on-phase-active` strategy and `register-validation-tools` primitive. Added `spawn-researcher-for-missing-tools` strategy (Architect review round 2).
- [x] **CoordinatorState deletion safety:** Confirmed safe. `work.attempt_count` is tracked natively on Work struct (Architect verified).
- [x] **Environment failure recovery:** Director handles this via escalation path. Can spawn Researcher or surface to user (Architect review round 2).
- [x] **Decomposition strategies:** Already exist in `resources/decompose/strategies/` (default.yml, classify.yml, coverage.yml, validate.yml, failure.yml). Engine loads from both `engine/strategies/` and `decompose/strategies/` paths. No gap (Architect round 3 - false alarm).
- [x] **Hierarchy completion strategies:** `complete-phases` and `complete-specs` already exist in `reconciliation.yml`. Removed duplicate strategies 6-7 from this doc (Architect round 3).
- [x] **AgentAction cleanup:** Dead variants (AssignAgent, AcceptBundle, ValidateDocument, EvaluateCoverage) added to Phase 8 deletion list. Remaining variants re-attributed to Director (Architect round 3).

## Open Questions

- [ ] Does the `spawn-agent` primitive handle the implementer's worktree base-ref resolution? Currently done in `run_agent_task()` - this code stays (it's in the executor, not the coordinator).
- [ ] The `ReviseParent` action also increments `bubble_up_count` on `CoordinatorState`. With `CoordinatorState` deleted, bubble-up depth tracking needs a new home - likely a counter on the Plan record itself.

## References

- [v4 Vision](../v4-vision.md) - the architecture this cutover completes
- [Primitive Vocabulary](2026-04-11-primitive-vocabulary.md) - 60 primitives (Doc 2)
- [FSM-in-YAML](2026-04-11-fsm-in-yaml.md) - runtime interpreter (Doc 3)
- [Trigger and Guard System](2026-04-11-trigger-guard-system.md) - 59 triggers (Doc 4)
- [Strategy Composition](2026-04-11-strategy-composition.md) - composition engine (Doc 5)
- [Decomposer as Strategy](2026-04-11-decomposer-as-strategy.md) - decomposition pipeline (Doc 6)
- [Hardcoded Knobs Inventory](../hardcoded-knobs-inventory.md) - every parameter being replaced
