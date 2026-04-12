# Architectural Review: v4 Vision & Strategy Composition

**Status:** REJECTED
**Reviewer:** Principal Architect

## 1. Define the Invariant
The v4 architectural vision establishes three core invariants:
1. **Single-Tick Constraint:** Strategies execute within one engine cycle. Multi-step flows must chain via events, not via suspend/resume or embedded multi-step logic.
2. **Composition, Not Scripting:** Orchestration policy lives in YAML. Rust primitives are strictly atomic.
3. **Idempotency & Self-Healing:** Because partial execution can occur before a daemon crash, triggers and action sequences must be safe to re-evaluate without duplicate side-effects.

## 2. Trace the Entry & Exit
Data enters via `DaemonEvent`s into the `ObservationCtx`. The `CompositionEngine` evaluates triggers on every tick, fires `StrategyDefinition`s, and sequences `Primitive` calls. Primitives mutate the TaskStore and Git worktree, outputting results into a transient `strategy_ctx`.

## 3. Hunt the Ghosts (Structural Violations)

The design documents present several severe structural contradictions that will result in state corruption, ghost tasks, and split-brain architectures.

### Flaw A: The "Decomposer Agent" is a Shadow Orchestrator
**Location:** `docs/design/2026-04-11-decomposer-as-strategy.md` (Doc 6)
**Violation:** The vision explicitly prohibits multi-step procedural workflow engines. Yet Doc 6 states: *"Inside the decomposer agent task: 1. Read pipeline config... 2. Execute stages in order... 5. Run validation... 6. Run ratification"*.
This offloads the entire decomposition pipeline into a background Rust task. You have merely hidden the procedural loop inside `spawn-agent(role: decomposer)`.
**Impact:** If the daemon crashes midway through decomposing phases, the agent task dies. The `decomposition-stalled` trigger will eventually restart it. Because the agent task contains an internal loop over the YAML pipeline, it must now implement its own resume-from-partial-state logic (checking which specs/phases already exist) in Rust. This entirely defeats the v4 purpose.

### Flaw B: Strategy Re-fire Idempotency Gap
**Location:** `docs/design/2026-04-11-primitive-vocabulary.md` (Doc 2) & `docs/design/2026-04-11-strategy-composition.md` (Doc 5)
**Violation:** A strategy's intermediate context (`$context`) only survives for one tick. If a strategy crashes after Step 1 (`spawn-agent`), Step 1 will re-run on the next tick because the trigger condition (e.g., `session-failure`) is still true. Doc 2 labels `spawn-agent` as `GuardRequired`, but the YAML schema in Doc 5 provides no mechanism to specify preconditions *per primitive* in an action sequence.
**Impact:** Ghost tasks and duplicate sessions. If you retry a work item and spawn an advisor agent, a crash before completion means the next tick spawns a *second* advisor agent.

### Flaw C: Git Worktree Concurrency
**Location:** `docs/design/2026-04-11-primitive-vocabulary.md` (Doc 2)
**Violation:** The document states that git-mutating primitives require exclusive access to the worktree, suggesting either an "internal git mutex" OR "scoping all git strategies to the same plan with priority ordering".
**Impact:** Priority ordering does *not* serialize git operations across different plans, nor does it protect against manual overrides. The git mutex must be a hard, centralized requirement inside `PrimitiveContext`. Relying on YAML priority ordering to prevent git corruption is a catastrophic blast radius failure.

### Flaw D: The Cooldown Memory Leak
**Location:** `docs/design/2026-04-11-trigger-guard-system.md` (Doc 4)
**Violation:** Cooldowns are tracked in a `HashMap<(String, String), Instant>`. The mitigation for unbounded growth is listed as "Periodic sweep... Or use an LRU cache."
**Impact:** An LRU cache drops the oldest entries. If a long-running system evicts a cooldown entry prematurely, the engine will re-trigger and cause a trigger storm. Cooldown pruning must be based on a deterministic TTL sweep linked to the engine tick, never an LRU eviction.

## 4. Scrutinize the Tests
The migration plan for `FsmInterpreter` (Doc 3) assumes that testing string-based FSM evaluation is equivalent to type-safe macro derivation.
**Test Reality:** You are passing hardcoded YAML keys as strings between Rust structs. The `FsmStatus` trait maps enum variants to kebab-case strings, but strategy YAML files pass literal strings to primitives. There is no proof in the design that a typo in a primitive param (e.g., `target-status: abondoned`) will be caught at startup unless the *entire* schema explicitly binds `target-status` parameters to valid FSM state enumerations during startup validation.

## 5. Execution Orders (Required Fixes)

Reject the implementation of the v4 engine until the following design corrections are explicitly detailed:

1. **Eradicate the Shadow Orchestrator:** If the single-tick constraint holds, the decomposer must be implemented as true single-tick strategies. For example: `decompose-plan-to-specs` fires on `plan-ready`. Its completion event `decomposition.level-complete` triggers `decompose-specs-to-phases`. If this is too slow, adjust the engine tick model—do not build a secret procedural orchestrator to bypass your own vision invariants.
2. **Mandate Primitive Guards:** Update the strategy YAML schema (Doc 5) to allow action sequences to include explicit guard assertions (e.g., `skip-if-exists`) for `GuardRequired` primitives.
3. **Hard Git Mutex:** Update Doc 2. The `PrimitiveContext` must acquire an asynchronous, centralized worktree lock before *any* git mutation. Priority ordering is disallowed as a concurrency control mechanism.
4. **Deterministic Cooldown Sweep:** Update Doc 4. The engine tick must formally clear expired `last_fired_at` entries. Remove all mentions of LRU caches.

Do not proceed with implementation until these architectural fractures are resolved.
