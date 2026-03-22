# Design Document: Loopr v3 MVP4 — Multi-Level RWL & Full Agent Roster

**Author:** Scott Aidler + Claude
**Date:** 2026-02-26
**Status:** Implemented
**Review Passes:** 10/10 (5 initial + 5 focused on new sections)

## Summary

MVP4 completes the Loopr vision by extending the Ralph Wiggum Loop from code-level only (Implementer + Reviewer) to all four levels of the hierarchy: Plan, Spec, Phase, and Code. It adds two new LLM agent personas (Coordinator, Researcher), a deterministic Integrator task (no LLM — the integration pipeline is fully mechanical), a generic context builder with token budgeting and learning selection, enriched Learning records with confidence scoring and role filtering, and document generation prompts that produce Plans, Specs, and Phases — not just validate them.

## Problem Statement

### Background

MVP1 proved the orchestration spine: daemon-as-single-authority, FSMs, TaskStore, IPC, worktrees, Ticks. MVP2 added read-only LLM intelligence via the Doc Validator. MVP3 added the Implementer and Reviewer agents, completing the code-level RWL: Work → code in worktree → Bundle → review → Tick.

But the upper levels of the pipeline remain entirely human-driven. A human must manually create Plans, Specs, Phases, and Works through the TUI. The "dev team in a box" has two team members (Implementer, Reviewer) and a paperwork checker (Validator). The Coordinator, Architect, Researcher, and Integrator seats are empty.

### Problem

The Ralph Wiggum Loop only operates at the code level. Plans, Specs, and Phases are created manually by the human Coordinator. There is no LLM-driven generation at these levels, no automated coordination of the pipeline, no research capability, and no automated Tick management. The full pipeline — Plan → Spec → Phase → Code → Review → Tick — requires constant human intervention at every stage above code.

Additionally, the current Learning system is flat: learnings are stored as strings with no confidence scoring, no role-based filtering, and no decay. The context builder (`load_context()`) is hardcoded for the Implementer role and cannot serve other agents or levels.

### Goals

- Coordinator agent that operates at all four levels (Plan/Spec/Phase/Code), creating hierarchy records, assigning work, and making decisions
- Document generation: LLM agents that produce Plans, Specs, and Phases (generate → validate → iterate)
- Researcher agent that searches codebases, reads files, and produces findings
- Deterministic Integrator task that automates Tick lifecycle (create/seal/validate/publish Ticks) — no LLM, pure state machine logic
- Generic context builder (`build_context()`) with per-role context slicing and token budgeting
- Enriched Learning model: confidence scoring, role applicability, resource tags (decay/GC deferred to MVP5)
- RWL at every level, each with its own system prompt, context assembly, and action set

### Non-Goals

- Cross-project coordination (multiple repos under one Coordinator) — future feature
- External web research (Researcher searches the local codebase only in this MVP)
- Network sandboxing / seccomp — premature
- UI for prompt editing (prompts are code, not user-configurable)

## Proposed Solution

### Overview

Four additions, built on the existing daemon/agent architecture:

1. **Context Builder** — A generic `build_context()` function that assembles role-specific, level-specific prompts from TaskStore state, with token budgeting and learning selection.

2. **Two New LLM Agents + One Deterministic Task** — Coordinator and Researcher follow the same LLM pattern as Implementer/Reviewer: system prompt + context loader + response parser + action executor. The Integrator is a deterministic Tokio task with no LLM — it executes a fixed state machine (the integration pipeline is fully mechanical: check for Accepted Bundles, create Tick, seal, validate, publish/fail). All three communicate via `AgentIpcBridge`.

3. **Document Generation Pipeline** — The Coordinator uses level-specific generation prompts to produce Plans, Specs, and Phases, then validates them via the existing Doc Validator before transitioning Draft → Active.

4. **Learning Enrichment** — Confidence scoring derived from reinforcements/contradictions, role applicability tags, resource tags for scoped selection, and age-based decay in context assembly.

### Architecture

#### Two Planes (unchanged from conversations)

| Plane | Parallelism | Agents | Mechanism |
|-------|-------------|--------|-----------|
| **Thinking** | High (Tokio tasks) | Coordinator, Researcher, Reviewer | No worktrees. Produce records. |
| **Changing** | Low (bounded pool) | Implementer | Worktrees. Write files, run tools, produce Bundles. |
| **Mixed** | Serial | Integrator | No worktree for decisions. Runs validation commands in repo root. |

#### Agent Roster (complete after MVP4)

| Agent | Level | Plane | Key Actions | Temperature | Max Iterations |
|-------|-------|-------|-------------|-------------|----------------|
| **Coordinator** | All | Thinking | CreatePlan, CreateSpec, CreatePhase, CreateWork, AssignAgent, SpawnResearcher, AcquireLock, ReleaseLock, ValidateDocument, TriageBundle, AcceptBundle, Transition, NeedHelp, Done | 0.2 | ∞ (long-lived) |
| **Researcher** | Any | Thinking | SearchCode, SearchFiles, ReadFile, ListDirectory, CreateLearning, Done | 0.1 | 10 |
| **Implementer** | Code | Changing | WriteFile, ReadFile, RunTool, Commit, ProposeBundle, CreateLearning, Done, NeedHelp | 0.3 | 20 |
| **Reviewer** | Code | Thinking | (single-shot review verdict) | 0.1 | 5 |
| **Integrator** | Code | Mixed | (deterministic — no LLM) CreateTick, SealTick, ValidateTick, PublishTick, FailTick, RejectBundle, CreateLearning | N/A | N/A |

**Why the Integrator is not an LLM agent:** The Integrator's logic is entirely deterministic: check for stuck Ticks, check for Accepted Bundles, create Tick, seal with Bundles, run validation commands, publish or fail. Every step is a straightforward if/then/else. Using an LLM to execute what is a 50-line Rust function would add complexity, latency, and API cost for zero benefit. LLM agents are reserved for tasks requiring language understanding and judgment (Coordinator, Implementer, Reviewer, Researcher).

**Note on Bundle transitions:** The existing Bundle FSM assigns `Proposed → Triaged` and `Reviewed → Accepted` to `Role::Coordinator`, not `Role::Integrator`. The Integrator can only perform `Accepted → Integrating → Merged` and `Integrating → Rejected`. Therefore, triage and acceptance of Bundles remain Coordinator actions. The Integrator handles the mechanical merge-validate-publish pipeline starting from Accepted Bundles.

#### The Coordinator Loop — The Heart of the Multi-Level RWL

The Coordinator is the meta-Ralph. Each iteration:

```
1. Load global state from TaskStore
   - Active AND Draft Plans, Specs, Phases (with status)
   - Works (with status and assignees)
   - Pending Bundles, recent Ticks
   - Active agent sessions (type, status, target, query)
   - Active Locks (resource, holder, status)
   - High-confidence Learnings (process-level)

2. Assess: what level needs attention?
   - No active Plan AND no Draft Plan in validation? → generate one
   - Active Plan, no Specs (and no Draft Specs in validation)? → generate Specs
   - Active Specs, no Phases (and no Draft Phases in validation)? → generate Phases
   - Active Phases, no Works? → create Works
   - Ready Works, Implementer pool not full? → acquire locks on resource_tags, spawn Implementers
   - Proposed/Triaged Bundles? → triage and route to Reviewers
   - Reviewed Bundles? → accept (transition Reviewed → Accepted)
   - All Works Done in an Active Phase? → mark Phase Complete
   - All Phases Complete in an Active Spec? → mark Spec Complete
   - Acceptance criteria met for Active Plan? → mark Plan Complete

3. Execute actions at the chosen level (ONE level per iteration)
   - Create/transition hierarchy records via IPC bridge
   - Spawn agents via `agent.start` IPC method
   - Create process-level Learnings

4. Fresh context dies. Next iteration starts clean.
```

**Draft-awareness rule:** The Coordinator checks for existing Draft records at each level before generating new ones. If a Draft Plan exists, the Coordinator validates or iterates on it rather than creating a duplicate. This prevents thrashing on generation.

**Status preconditions:** Hierarchy transitions are gated by both child completion AND current status. "All Works Done in a Phase" only triggers `Phase → Complete` if the Phase is currently `Active`. The FSM rejects invalid transitions, but the Coordinator avoids wasted iterations by checking status preconditions in the prompt context.

The Coordinator does NOT maintain conversation state. It reads TaskStore each iteration, sees the world fresh, and decides what to do next. This is the Ralph Wiggum principle applied to coordination itself.

#### Coordinator Scheduling

The Coordinator runs on an **adaptive timer**. After an iteration that performed actions, the next iteration starts after `active_interval_secs` (default: 5 seconds). After an iteration that emitted `Done` with nothing to do, the interval stretches to `idle_interval_secs` (default: 30 seconds). This provides responsive coordination during active work without wasting LLM calls during idle periods.

```rust
pub struct CoordinatorConfig {
    // ... inherits AgentRoleConfig fields ...
    pub active_interval_secs: u64,       // default 5
    pub idle_interval_secs: u64,         // default 30
    pub max_validation_attempts: u32,    // default 3
}
```

If the Coordinator session reaches `Failed` status (LLM error, parse failure, etc.), the daemon auto-restarts it after `idle_interval_secs * 2`. The Coordinator reads fresh state on each iteration, so restart is always safe.

**Coordinator loop structure:**
```rust
pub async fn run_coordinator(session, stores, bridge, config, event_tx) -> Result<()> {
    loop {
        // 1. Check cancellation (re-read session from Stores)
        if session_is_cancelled(stores, &session.id)? { return Ok(()); }

        // 2. Load context, call LLM, parse actions, execute
        let outcome = run_coordinator_iteration(&session, stores, bridge, config, event_tx).await;

        // 3. Determine interval
        let interval = match &outcome {
            Ok(IterationOutcome::Done(_)) => config.idle_interval_secs,
            Ok(IterationOutcome::Continue(_)) => config.active_interval_secs,
            Ok(IterationOutcome::NeedHelp(_)) => return outcome_to_result(outcome),
            Err(_) => return outcome_to_result(outcome),
        };

        // 4. Sleep
        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
}
```

This is a long-lived Tokio task, unlike Implementer/Reviewer which run a fixed iteration count and exit. The Coordinator runs until cancelled or until it emits `NeedHelp`.

#### User Intent Ingestion

The Coordinator requires a goal to generate Plans. This is provided via a new IPC method:

```
coordinator.set_goal { goal: String }
coordinator.clear_goal {}
```

Callable from the TUI (keybinding `g`) or CLI (`loopr coordinator set-goal "Build feature X"`). The goal is persisted in TaskStore as a singleton `CoordinatorGoal` record (implements `Record` trait). This preserves the MVP1 invariant that all meaningful state is in TaskStore and survives daemon crashes. When the Coordinator sees no active Plan and a goal record exists, it generates a Plan from the goal. If no goal exists, the Coordinator operates on existing hierarchy only (managing Works, Bundles, etc.) and does not generate new Plans.

#### Document Generation Pipeline

When the Coordinator decides to generate a document (Plan, Spec, or Phase), it follows this cycle:

```
Generate (LLM) → Create as Draft → Validate (Doc Validator) →
  if Pass/Warn → Transition to Active
  if Fail → Read validation issues → Re-generate with ALL previous issues as context → loop
  if max_validation_attempts reached → NeedHelp (pause, notify human)
```

The `max_validation_attempts` (default 3) caps the loop. ALL previous validation failure messages are included in re-generation context (not just the most recent), preventing oscillation between failure modes. After the cap, the Coordinator emits `NeedHelp` and the Draft remains for human review.

**`ValidateDocument` return semantics:** When the executor processes `ValidateDocument`, it calls the existing Doc Validator via the IPC bridge (`validator.validate`) and returns `ActionResult::ValidationResult { passed: bool, issues: Vec<String> }`. The Coordinator receives this result within the same iteration and can act on it immediately (e.g., transition to Active if passed, or prepare re-generation context if failed).

**What "one iteration" means for the Coordinator:** One iteration = one LLM call that returns a JSON array of actions, all executed sequentially. The generate-validate loop spans multiple iterations: iteration N creates a Draft, iteration N+1 validates it (the validation issues appear in the context because the Draft exists and has a failed report), iteration N+2 re-generates with issues. The "one level per iteration" rule means the Coordinator does not generate a Spec in the same iteration it marks a Phase complete — it picks one level to focus on.

#### Convergence Criteria Per Level

| Level | Generation Complete When | Validation Criteria |
|-------|------------------------|---------------------|
| **Plan** | One Plan passes Doc Validator and transitions to Active | Clear objective, measurable acceptance criteria, bounded scope (existing `plan_prompt()` rubric) |
| **Spec** | One Spec per Plan passes Doc Validator and transitions to Active | References Plan, technical approach described, decisions documented, testability addressed (existing `spec_prompt()` rubric) |
| **Phase** | All Phases for a Spec pass Doc Validator and transition to Active; together they cover the Spec's scope | References Spec, ordered correctly, concrete deliverables, dependencies identified (existing `phase_prompt()` rubric) |
| **Work** | All Works for a Phase created; each is small enough for ~5-10 Implementer iterations | No Doc Validator gate (Works are operational, not documents). Coordinator judgment. |

The generation prompts are distinct from the validation prompts:

| Prompt | Input | Output |
|--------|-------|--------|
| **Plan Generation** | User intent / high-level goal, relevant Learnings | Plan title, description, acceptance criteria |
| **Spec Generation** | Active Plan, relevant Learnings, codebase findings | Spec title, description (technical approach, decisions, testability) |
| **Phase Generation** | Active Spec, relevant Learnings | Ordered list of Phase records (title, description, order, deliverables, dependencies) |
| **Work Generation** | Active Phase, relevant Learnings, codebase findings | Work records (title, description, resource tags, dependencies) |

Each generation prompt also receives any previous validation failures as context, enabling the generate-validate-iterate loop.

#### The Researcher

The Researcher is a focused agent that searches and summarizes. The Coordinator spawns Researchers when it needs information before making a decision:

- "What patterns does this codebase use for error handling?" → Researcher searches, reads files, produces a Learning
- "What modules would be affected by this change?" → Researcher greps for imports/references, produces a Learning
- "What does the existing test coverage look like for this module?" → Researcher reads test files, summarizes

The Researcher's output is always a Learning (scoped to the relevant Plan/Spec/Phase/Work). The Coordinator picks up these Learnings on its next iteration.

**Researcher Actions:**
- `SearchCode { pattern, glob, path }` — regex search via ripgrep (same as `Grep` semantics). Path must be within repo root.
- `SearchFiles { pattern, path }` — glob pattern search using `glob` crate (NOT `find`). Path must be within repo root.
- `ReadFile { path }` — read a file from the repo (not a worktree). **Path sandboxed:** canonicalized and validated to be within repo root. Absolute paths rejected. Symlink following disabled.
- `ListDirectory { path }` — list files in a directory. Same sandboxing as `ReadFile`.
- `CreateLearning { scope, content, resource_tags }` — persist a finding as a Learning
- `Done { summary }` — complete with a summary of findings

The Researcher runs in the repo root (read-only, no worktree). It cannot write files or run tools.

**Researcher deduplication:** When the Coordinator spawns a Researcher via `SpawnResearcher { query, scope_id }`, the handler checks for an existing non-terminal Researcher session with the same `scope_id`. If one exists, the spawn is rejected with an informative error. The Coordinator's context includes active Researcher sessions with their queries, so the LLM can avoid redundant spawns.

**Security: file access sandboxing.** All Researcher file operations (ReadFile, SearchCode, SearchFiles, ListDirectory) apply the same path canonicalization and containment check used by `WriteFile` in `executor.rs`. Paths are:
1. Rejected if absolute
2. Joined to repo root
3. Canonicalized via `std::fs::canonicalize()`
4. Validated to start with the repo root path
5. Checked against a denylist: `.env`, `*.key`, `*.pem`, `credentials.*`, `*secret*`

For `SearchCode` wrapping `rg`: the `--no-follow` flag is always set to prevent symlink escapes.

#### Advisory Lock System

Locks already exist in the codebase (`src/domain/lock.rs`): `Lock { resource, holder_id, granted_by, status: Active|Released|Expired }` with IPC methods `lock.acquire` and `lock.release`. MVP4 wires them into the agent lifecycle:

**Coordinator grants locks:** When the Coordinator assigns an Implementer to a Work, it acquires locks on the Work's `resource_tags` (file paths, module names). The Coordinator is the `granted_by` authority; the Work ID is the `holder_id`.

**Executor checks locks on WriteFile:** When `ConflictPolicy::LockStrict` is active, `execute_action()` for `WriteFile` checks the Stores for active Locks on the target file path. If the path is locked by a different `holder_id` than the current agent's `work_id`, the write is rejected with `ActionResult::ActionError("file locked by work X")`. The Implementer can then `NeedHelp` or work on a different file. With `ConflictPolicy::LockAdvisory`, the check is skipped and conflicts are detected at Bundle merge time by the Integrator.

**Coordinator releases locks:** When a Work reaches `Done` or `Abandoned`, the Coordinator releases its locks.

**Conflict policy** is configurable (see Strategy Knobs below):
- `LOCK_ADVISORY` (default): Locks are checked but not enforced at the file-write level. Conflicts detected at Bundle merge time by the Integrator.
- `LOCK_STRICT`: File writes to locked paths are rejected by the executor.

**Lock expiry:** Locks are acquired with a TTL (default: `max_lock_ttl_minutes: 60` in `StrategyConfig`). On each Coordinator iteration, expired locks are automatically released. On daemon restart, crash recovery checks for locks whose `created_at + ttl` has passed and expires them. This prevents stuck locks from a Coordinator crash.

**Lock granularity:** Locks are on individual file paths, not directories. A Work with `resource_tags: ["src/agents/mod.rs", "src/agents/coordinator.rs"]` acquires two locks. If the Coordinator needs to lock an entire directory, it acquires locks on each file the Work will touch (based on resource_tags).

**Coordinator lock actions:**
```rust
AcquireLock { resource: String, holder_id: String },
ReleaseLock { lock_id: String },
```

These are added to the Coordinator's action set and system prompt.

#### Spec/Design Proposers (Lightweight Swarm)

For Spec and Phase generation, the Coordinator can optionally spawn multiple **Proposer** agents — a variant of Researcher that produces draft document content instead of findings. The Coordinator then selects the best proposal.

**How it works:**
1. Coordinator decides a Spec is needed for an active Plan
2. Coordinator spawns 2-3 Proposers (same pool as Researchers, `pool_size: 4`) with the query "Propose a Spec for Plan X"
3. Each Proposer investigates the codebase, produces a draft Spec as a Learning (scoped to the Plan, tagged `proposer:spec`)
4. On the next iteration, the Coordinator sees 2-3 proposal Learnings, selects the best one, and creates a Spec from it
5. The Spec goes through the normal validate-iterate cycle

**This is NOT a new agent type.** A Proposer is a Researcher with a different query prompt. The Researcher system prompt already handles "investigate and create a Learning." The query simply changes from "What patterns exist?" to "Propose a technical approach for achieving X." The Coordinator decides whether to use direct generation (single LLM call) or proposer spawning (multi-agent) based on complexity — this is a judgment call in the Coordinator's prompt, not a hard rule.

**Proposal vs. finding Learnings:** Proposals are distinguished by a `resource_tags` convention: `["proposal:spec", "plan:{plan_id}"]`. The Coordinator's context builder includes a `proposals_for(plan_id)` helper that filters Learnings by this tag pattern. Regular research findings use tags like `["finding", "module:src/agents"]`. The tag convention is documented in the Researcher system prompt so the LLM tags correctly.

**Proposer vs. direct generation:** For simple Plans/Works, the Coordinator generates directly (one LLM call). For complex Specs and Phase breakdowns, spawning 2-3 Proposers produces higher quality through diversity. The Coordinator prompt includes guidance: "For Specs with significant design decisions, spawn 2-3 proposers rather than generating directly."

#### Iteration Records

Each agent iteration produces a structured Learning that serves as the Iteration record:

```json
{
  "scope": "work",
  "source_id": "wi-123",
  "content": "## Iteration 5\n**Outcome:** continue\n**Actions:** wrote src/foo.rs, ran tests (pass), committed\n**Next:** implement error handling\n**Blockers:** none",
  "applicable_roles": ["coordinator"],
  "resource_tags": ["iteration:5", "agent:impl-session-abc"]
}
```

The key insight: an Iteration record IS a Learning. It has scope, source, content, and tags. The `resource_tags` include `iteration:N` and `agent:session-id` for filtering. The `applicable_roles: ["coordinator"]` ensures only the Coordinator sees iteration summaries (other agents don't need them).

The Coordinator uses iteration records to track progress: "Implementer on Work X completed iteration 5, wrote foo.rs, tests pass, next is error handling." This informs Coordinator decisions about whether to wait, reassign, or mark done.

**No new record type needed.** The Learning model already has scope, source_id, applicable_roles, and resource_tags. Iteration records are just a structured-content convention.

**Iteration record lifecycle:** Iteration Learnings are ephemeral. When a Work reaches `Done` or `Abandoned`, the Coordinator releases locks AND archives iteration records for that Work (transition to a low-confidence state so they fall out of `select_learnings` results). This prevents unbounded growth. Only the most recent iteration record per agent session is kept at full confidence; older ones are superseded.

#### The Integrator Task (Deterministic — No LLM)

The Integrator is a **deterministic Tokio task**, not an LLM agent. Its logic is a pure state machine — every decision is an if/then/else on data from TaskStore. No prompt engineering, no response parsing, no temperature tuning.

```rust
pub async fn run_integrator(
    stores: &Stores,
    bridge: &AgentIpcBridge,
    config: &IntegratorConfig,
    event_tx: &broadcast::Sender<DaemonEvent>,
) -> Result<()> {
    loop {
        // 1. Check cancellation
        if session_is_cancelled(stores, &session.id)? { return Ok(()); }

        // 2. Recover stuck Ticks (from crash)
        for tick in ticks_in_state(stores, [Sealing, Validating]) {
            fail_tick(bridge, &tick.id, "recovered from crash")?;
            create_learning(bridge, "integration", &tick.id, "Tick stuck after crash, failed")?;
        }

        // 3. Check for Accepted Bundles
        let accepted = bundles_in_state(stores, Accepted);
        if !accepted.is_empty() && no_tick_in_progress(stores) {
            // 3a. Validate preconditions BEFORE any mutations (validate-then-mutate)
            let latest_tick = latest_published_tick(stores);
            let valid_bundles: Vec<_> = accepted.iter()
                .filter(|b| b.base_tick_id == latest_tick.map(|t| &t.id))
                .collect();
            // Reject stale bundles
            for stale in accepted.iter().filter(|b| !valid_bundles.contains(b)) {
                reject_bundle(bridge, &stale.id, "stale base_tick_id")?;
            }
            if !valid_bundles.is_empty() {
                // 3b. Create and seal Tick (atomically transitions Bundles → Integrating)
                let tick_id = create_tick(bridge)?;
                let bundle_ids: Vec<_> = valid_bundles.iter().map(|b| &b.id).collect();
                seal_tick(bridge, &tick_id, &bundle_ids)?;

                // 3c. Merge branches, run validation commands
                let merge_sha = merge_bundles(config, &valid_bundles)?;
                let validation = run_validation_commands(config).await;

                if validation.passed {
                    publish_tick(bridge, &tick_id, &merge_sha)?;
                } else {
                    fail_tick(bridge, &tick_id, &validation.output)?;
                    for b in &valid_bundles {
                        reject_bundle(bridge, &b.id, &validation.output)?;
                    }
                    create_learning(bridge, "integration", &tick_id,
                        &format!("Tick validation failed: {}", validation.output))?;
                }
            }
        }

        // 4. Sleep
        tokio::time::sleep(Duration::from_secs(config.interval_secs)).await;
    }
}
```

**Where the SHA comes from:** The `merge_bundles()` function performs `git merge` of each Bundle's branch into the integration branch and returns the resulting merge commit SHA. This SHA is then passed to `publish_tick()`.

**SealTick validate-then-mutate:** Before performing any mutations, the Integrator validates all preconditions: each Bundle must be in `Accepted` state, and each Bundle's `base_tick_id` must match the latest published Tick. Only after all validations pass does it transition the Tick to `Sealing` and each Bundle to `Integrating`. This eliminates the partial-failure window for non-crash scenarios.

**Note:** Triage (`Proposed → Triaged`), review routing, and acceptance (`Reviewed → Accepted`) are Coordinator responsibilities, not Integrator. The existing Bundle FSM assigns these transitions to `Role::Coordinator`. The Integrator only handles the pipeline from Accepted onward.

**Crash recovery for stuck Ticks:** On daemon restart, `recover_orphaned_records()` in `DaemonContext` is extended to detect Ticks stuck in `Sealing` or `Validating` state and transition them to `Failed`. The Integrator also checks for this at the start of each cycle.

**Integrator config** uses the existing `IntegratorConfig` struct (already defined in MVP1 with `validation_commands: Vec<String>`), extended with:
```rust
pub struct IntegratorConfig {
    pub validation_commands: Vec<String>,  // existing
    pub interval_secs: u64,               // NEW — default 15
    pub enabled: bool,                    // NEW — default false
}
```

### Data Model

#### Role Enum Extension

```rust
pub enum Role {
    Coordinator,
    Integrator,
    Implementer,
    Reviewer,
    Researcher,  // NEW
}
```

The Researcher role is needed for:
- `Learning.applicable_roles` filtering (so Learnings can target Researchers specifically)
- IPC bridge role identification (Researcher creates Learnings with `Role::Researcher`)
- Future FSM rules if Researchers gain state-mutating capabilities

#### AgentSession Extension

```rust
pub struct AgentSession {
    // ... existing fields (id, agent_type, work_id, bundle_id, status,
    //                      iteration, model, worktree_path, error_message, timestamps) ...

    /// Generic target ID for agents that don't target Works or Bundles.
    /// Coordinator: None (operates globally).
    /// Researcher: the scope_id (Plan/Spec/Phase/Work ID being researched).
    /// Integrator: None (operates on whatever Accepted Bundles exist).
    #[serde(default)]
    pub target_id: Option<String>,

    /// Query string for Researcher agents. Set by SpawnResearcher action.
    #[serde(default)]
    pub query: Option<String>,
}
```

**Handler validation per agent type:**
- `Implementer`: requires `work_id`
- `Reviewer`: requires `bundle_id`
- `Coordinator`: no target required (reject if one already running — pool_size = 1)
- `Researcher`: requires `target_id` (scope_id) and `query`; `target_id` must exist in TaskStore
- `Integrator`: no target required (reject if one already running — pool_size = 1)

#### Learning Enrichment

Current `Learning` fields (unchanged):
```rust
pub struct Learning {
    pub id: String,
    pub source_id: String,
    pub scope: LearningScope,
    pub content: String,
    pub reinforcements: u32,
    pub contradictions: u32,
    pub promoted: bool,
    pub created_at: i64,
    pub updated_at: i64,
}
```

New fields added (all with `#[serde(default)]` for backward compatibility with existing JSONL):
```rust
pub struct Learning {
    // ... existing fields ...

    /// Roles this learning is relevant to. None = all roles.
    #[serde(default)]
    pub applicable_roles: Option<Vec<Role>>,

    /// Resource tags for scoped selection (file paths, module names).
    #[serde(default)]
    pub resource_tags: Vec<String>,

    /// Computed confidence: reinforcements / (reinforcements + contradictions).
    /// Updated on reinforce() / contradict(). Range 0.0..=1.0.
    /// Default 0.5 for new learnings (neutral).
    #[serde(default = "default_confidence")]
    pub confidence: f32,
}

fn default_confidence() -> f32 { 0.5 }
```

**Backward compatibility:** Existing JSONL records missing these fields will deserialize with defaults: `applicable_roles = None` (all roles), `resource_tags = []`, `confidence = 0.5`. A migration test verifies: "deserialize a Learning JSON string from the pre-MVP4 format and verify defaults are applied."

**Confidence computation:**
```rust
pub fn recompute_confidence(&mut self) {
    let total = self.reinforcements + self.contradictions;
    self.confidence = if total == 0 {
        0.5
    } else {
        (self.reinforcements as f32 / total as f32).clamp(0.0, 1.0)
    };
}
```

**Indexed fields** updated to include `confidence` for efficient querying.

#### AgentAction Extensions

**AgentAction architecture note:** MVP3's `AgentAction` has 9 variants. MVP4 adds ~12 more, most valid for only one agent type. To prevent a god-type and enable compile-time enforcement, each agent module defines its own action enum (`CoordinatorAction`, `ResearcherAction`) that converts `Into<AgentAction>` for execution. Each agent's parser only produces its own action type. The executor validates that the action is allowed for the current `AgentType` before executing.

**Coordinator-specific actions:**
```rust
pub enum AgentAction {
    // ... existing actions (RunTool, WriteFile, ReadFile, Commit, ProposeBundle,
    //                       Transition, CreateLearning, Done, NeedHelp) ...

    CreatePlan { title: String, description: String, acceptance_criteria: String },
    CreateSpec { plan_id: String, title: String, description: String },
    CreatePhase { spec_id: String, title: String, description: String, order: u32 },
    CreateWork { phase_id: String, title: String, description: String },
    AssignAgent { agent_type: String, target_id: String },
    SpawnResearcher { query: String, scope_id: String },
    ValidateDocument { collection: String, id: String },
    AcquireLock { resource: String, holder_id: String },
    ReleaseLock { lock_id: String },
    // TriageBundle and AcceptBundle are syntactic sugar for Transition actions,
    // but defined as distinct variants so the Coordinator prompt can use clear names
    // and the executor can enforce Role::Coordinator for these specific transitions.
    TriageBundle { bundle_id: String },     // Proposed → Triaged
    AcceptBundle { bundle_id: String },     // Reviewed → Accepted
}
```

**`SpawnResearcher` vs `AssignAgent`:** These are separate actions because `SpawnResearcher` carries a `query` field that is stored on the `AgentSession` and drives the Researcher's investigation. `AssignAgent` is used for Implementers (needs `work_id`) and Reviewers (needs `bundle_id`). Unifying them would require an overloaded `params` map that obscures the distinct contracts. Internally, both call `agent.start` via the IPC bridge.

**`AssignAgent` semantics:** When the executor processes `AssignAgent`:
1. Validates `agent_type` is "implementer" or "reviewer"
2. Checks pool_size for the target type (reject with error if pool is full)
3. Creates `AgentSession` with appropriate target field (`work_id` for Implementer, `bundle_id` for Reviewer)
4. Calls `agent.start` via IPC bridge
5. Returns `ActionResult::AgentAssigned { session_id, agent_type }` or `ActionResult::ActionError` if pool is full

**Researcher-specific actions:**
```rust
SearchCode { pattern: String, glob: Option<String>, path: Option<String> },
SearchFiles { pattern: String, path: Option<String> },
ListDirectory { path: String },
```

**Integrator IPC bridge calls** (not `AgentAction` variants — the Integrator calls these directly as IPC methods, not via LLM-parsed actions):
- `tick.create` → creates Tick (Open)
- `tick.transition` → Sealing, Validating, Published, Failed
- `bundle.transition` → Integrating, Merged, Rejected
- `learning.create` → failure insights
These use `Role::Integrator` through the bridge.

**`Transition` action — role field added:**
```rust
Transition {
    collection: String,
    id: String,
    target_state: String,
    role: Option<String>,  // NEW — if None, inferred from agent_type
}
```

The executor infers role from agent_type when `role` is None:
- `AgentType::Coordinator` → `Role::Coordinator`
- `AgentType::Integrator` → `Role::Integrator`
- `AgentType::Implementer` → `Role::Implementer`
- `AgentType::Reviewer` → `Role::Reviewer`
- `AgentType::Researcher` → `Role::Researcher`

This fixes the existing bug where `Transition` always defaulted to whatever the handler chose, which would cause Integrator bundle transitions to fail.

**`CreateLearning` — extended fields:**
```rust
CreateLearning {
    content: String,
    scope: String,
    source_id: String,
    applicable_roles: Option<Vec<String>>,  // NEW
    resource_tags: Option<Vec<String>>,      // NEW
}
```

If `applicable_roles` and `resource_tags` are not provided by the LLM, the executor infers defaults:
- `applicable_roles`: derived from the creating agent's type (e.g., Implementer → `[Implementer]`, Coordinator → `None` meaning all roles)
- `resource_tags`: derived from the Work's `resource_tags` if the agent has a `work_id`

### Context Builder — `build_context()`

The generic context builder replaces the hardcoded `load_context()` in `implementer.rs` and `load_review_context()` in `reviewer.rs`.

```rust
/// Role-agnostic context assembly with token budgeting.
pub struct ContextBuilder<'a> {
    stores: &'a Stores,
    role: Role,
    agent_type: AgentType,
    budget: TokenBudget,
}

/// Token budget allocation per context section.
/// Token estimation: ~1.3 tokens per word (~4 characters per token).
pub struct TokenBudget {
    pub system_prompt: usize,
    pub work_target: usize,
    pub hierarchy: usize,
    pub learnings: usize,
    pub state_summary: usize,
    pub tools_or_actions: usize,
    pub previous_summary: usize,
}

pub struct AssembledContext {
    pub system_prompt: String,
    pub user_message: String,
    pub token_estimate: usize,
}
```

**Per-role token budgets:**

| Section | Coordinator | Researcher | Implementer | Reviewer | Integrator |
|---------|-------------|------------|-------------|----------|------------|
| system_prompt | 800 | 500 | 500 | 500 | 600 |
| work_target | 500 | 1000 | 1000 | 1000 | 500 |
| hierarchy | 3000 | 1000 | 2000 | 2000 | 500 |
| learnings | 1500 | 1000 | 2000 | 2000 | 1000 |
| state_summary | 3000 | 0 | 2000 | 1000 | 1500 |
| tools_or_actions | 500 | 300 | 500 | 0 | 400 |
| previous_summary | 700 | 700 | 1000 | 0 | 500 |
| **Total budget** | **10000** | **4500** | **9000** | **6500** | **5000** |

**Truncation strategy:** When a section exceeds its token budget:
- For structured lists (Works, Learnings, agent sessions): drop items from the end (lowest priority) until under budget. Never truncate an item mid-text.
- For prose (descriptions, summaries): truncate at the last complete sentence boundary, append `[truncated]`.
- Log a warning when truncation occurs.

**Lock snapshot pattern:** The context builder acquires each collection's read lock briefly, clones the needed records into a local struct, then releases the lock before doing string formatting or token counting. This prevents holding read locks across the entire context assembly (which would block handlers needing write locks). The pattern:
```rust
let plans: Vec<Plan> = { let guard = stores.plans.read().unwrap(); guard.values().cloned().collect() };
// guard released here, formatting happens on local clones
```

**`scope_ids` construction per role:** The `select_learnings` function takes a `scope_ids` chain that identifies the hierarchy path. Each role constructs this differently:
- **Implementer:** `[(work_id, Work), (phase_id, Phase), (spec_id, Spec), (plan_id, Plan)]` — the full chain from Work up to Plan, traversed during context loading
- **Reviewer:** same chain as Implementer (loaded from Bundle → Work → Phase → Spec → Plan)
- **Coordinator:** `[(plan_id, Plan)]` for the active Plan, or `[]` for global-only when assessing
- **Researcher:** `[(scope_id, scope_type)]` — the single scope the Researcher was spawned for

**Learning selection query** (the key improvement):

```rust
pub fn select_learnings(
    stores: &Stores,
    scope_ids: &[(&str, LearningScope)],  // (id, scope) pairs up the hierarchy
    role: Role,
    min_confidence: f32,                   // default 0.3
    max_count: usize,                      // default 20
) -> Vec<&Learning> {
    let now = now_millis();
    let week_ms: i64 = 7 * 24 * 60 * 60 * 1000;

    learnings
        .values()
        .filter(|l| {
            // Scope match: this item or any ancestor, or Global
            scope_ids.iter().any(|(id, scope)| l.source_id == *id && l.scope == *scope)
                || l.scope == LearningScope::Global
        })
        .filter(|l| {
            // Role match: applicable to this role, or applicable to all
            l.applicable_roles.as_ref()
                .map(|roles| roles.contains(&role))
                .unwrap_or(true)
        })
        .filter(|l| {
            // Confidence match OR promoted (policies always included)
            let age_weeks = (now - l.updated_at) / week_ms;
            let decay = if l.reinforcements == 0 && age_weeks > 1 {
                (age_weeks - 1) as f32 * 0.1  // -0.1 per week after first week, unreinforced only
            } else {
                0.0
            };
            let effective_confidence = (l.confidence - decay).clamp(0.0, 1.0);
            l.promoted || effective_confidence >= min_confidence
        })
        .sorted_by(|a, b| {
            // Policies first, then by confidence DESC, then by recency DESC
            b.promoted.cmp(&a.promoted)
                .then(b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal))
                .then(b.updated_at.cmp(&a.updated_at))
        })
        .unique_by(|l| &l.id)  // deduplicate (promoted learnings won't appear twice)
        .take(max_count)
        .collect()
}
```

**MVP4 simplification:** The age-based decay formula and Learning GC (garbage collection with 500-record cap) are deferred to MVP5. MVP4 ships the data model fields (`applicable_roles`, `resource_tags`, `confidence`) and the `select_learnings()` function filters by scope, role, and confidence threshold. But we do not yet have enough Learnings in production to know whether decay or GC are needed. Ship the infrastructure, tune later.

**MVP5 decay formula (designed, not implemented in MVP4):** Learnings older than 7 days with zero reinforcements get a confidence penalty of `-0.1 * (age_weeks - 1)`. Reinforced learnings do not decay. Promoted learnings (policies) are always included.

#### Per-Role Context Slicing

| Role | Hierarchy Loaded | State Loaded | Learnings Filter |
|------|-----------------|--------------|-----------------|
| **Coordinator** | All active + Draft Plans/Specs/Phases (summary: title, status, id) | Work statuses, Bundle statuses, Tick status, active agent sessions (type, status, target, query) | Process learnings, ≥0.6 confidence |
| **Researcher** | Target scope only (the Plan/Spec/Phase being researched) | None | Code learnings scoped to resource tags, ≥0.3 confidence |
| **Implementer** | Full chain: Plan → Spec → Phase → Work (titles + descriptions) | Git worktree state (diff --stat, log) | All scoped learnings ≥0.3 confidence |
| **Reviewer** | Full chain: Plan → Spec → Phase → Work + Bundle | Bundle diff, touched paths | Module-scoped learnings ≥0.3 confidence |
| **Integrator** | None (operates on Bundles/Ticks directly) | Accepted Bundles (id, work_id, base_tick_id, touched_paths), current Tick, recent failures | Failure + integration learnings ≥0.3 confidence |

### Prompt Design

#### Coordinator System Prompt

```
You are the Coordinator agent in the Loopr development orchestrator. You are the
project manager and engineering manager. You own the full pipeline: Plan → Spec →
Phase → Work → Bundle → Tick.

## Your Responsibilities

1. Assess the current state of the project
2. Decide what level needs attention (Plan, Spec, Phase, or Code)
3. Create hierarchy records (Plans, Specs, Phases, Works)
4. Triage and accept Bundles (Proposed→Triaged, Reviewed→Accepted)
5. Assign work to Implementer and Reviewer agents
6. Manage resource locks (acquire before assignment, release on completion)
7. Spawn Researchers or Proposers when you need information or design options
8. Validate documents before activating them
9. Track progress and mark completed items
10. Create process-level Learnings

## Your Capabilities

You can perform the following actions (respond with a JSON array):

1. `create_plan` — Create a new Plan (Draft)
   {"action": "create_plan", "title": "...", "description": "...", "acceptance_criteria": "..."}
2. `create_spec` — Create a Spec under a Plan (Draft)
   {"action": "create_spec", "plan_id": "...", "title": "...", "description": "..."}
3. `create_phase` — Create a Phase under a Spec (Draft)
   {"action": "create_phase", "spec_id": "...", "title": "...", "description": "...", "order": 1}
4. `create_work` — Create a Work under a Phase (Draft)
   {"action": "create_work", "phase_id": "...", "title": "...", "description": "..."}
5. `assign_agent` — Start an Implementer or Reviewer on a target
   {"action": "assign_agent", "agent_type": "implementer", "target_id": "work-id"}
6. `spawn_researcher` — Investigate a question or propose a design
   {"action": "spawn_researcher", "query": "What patterns...", "scope_id": "spec-id"}
   For Spec proposals: {"action": "spawn_researcher", "query": "Propose a Spec for...", "scope_id": "plan-id"}
7. `acquire_lock` — Lock a resource before assigning work
   {"action": "acquire_lock", "resource": "src/agents/mod.rs", "holder_id": "work-id"}
8. `release_lock` — Release a resource lock when work is complete
   {"action": "release_lock", "lock_id": "lock-id"}
9. `validate_document` — Run the Doc Validator on a Draft Plan/Spec/Phase
    {"action": "validate_document", "collection": "plans", "id": "plan-id"}
10. `triage_bundle` — Move a Bundle from Proposed to Triaged
    {"action": "triage_bundle", "bundle_id": "..."}
11. `accept_bundle` — Move a reviewed Bundle to Accepted
    {"action": "accept_bundle", "bundle_id": "..."}
12. `transition` — Transition a record's status (Draft→Active, Active→Complete, etc.)
    {"action": "transition", "collection": "plans", "id": "...", "target_state": "active"}
13. `create_learning` — Record a process insight
    {"action": "create_learning", "content": "...", "scope": "plan", "source_id": "..."}
14. `need_help` — Request human intervention
    {"action": "need_help", "reason": "Cannot resolve validation failures after 3 attempts"}
15. `done` — Signal this iteration is complete
    {"action": "done", "summary": "Generated Spec for Plan X, spawned Researcher for Y"}

## Rules

- Operate at ONE level per iteration. Don't try to advance all levels at once.
- Check for existing Drafts before generating new documents. Iterate on existing Drafts.
- Always validate documents before transitioning Draft → Active.
- Create Works that are small enough to fit in half a context window.
- Don't assign more agents than the pool_size allows (check active sessions).
- Acquire locks on resource_tags BEFORE assigning Implementers. Release locks when Works complete.
- For Specs with significant design decisions, spawn 2-3 proposers rather than generating directly.
- When acceptance criteria are met, mark the Plan Complete.

## Output Format

Respond with ONLY a JSON array of actions.
```

**JSON parsing note:** All LLM agent response parsers (Coordinator, Researcher, Implementer, Reviewer) use the same extraction logic from MVP3's `parse_actions()`: try direct JSON parse first, then scan for the first `[...]` array in the response (handles markdown code blocks and chain-of-thought reasoning before the JSON). The "ONLY a JSON array" instruction is guidance to minimize extraneous text, not a strict requirement — the parser is robust to prose before/after the JSON.

#### Plan Generation Prompt (user message, level-specific)

```
## Current State

No active Plan exists. Create one based on the following context.

## User Intent

{user_provided_goal_or_objective}

## Relevant Learnings

{learnings from previous Plans, Global scope}

{if previous_validation_failures:}
## Previous Validation Failures (fix these)

{all accumulated validation issues}

## Instructions

Generate a Plan with:
- A clear, bounded title
- A description explaining what this Plan achieves and why
- Measurable acceptance criteria (specific, testable conditions)

Respond with a JSON array containing a single `create_plan` action.
```

#### Spec Generation Prompt (user message, level-specific)

```
## Active Plan

**{plan.title}** (ID: {plan.id})
{plan.description}

Acceptance Criteria: {plan.acceptance_criteria}

## Relevant Learnings

{learnings scoped to this Plan + Global}

## Codebase Findings

{researcher findings if available}

{if previous_validation_failures:}
## Previous Validation Failures (fix these)

{all accumulated validation issues}

## Instructions

Generate a Spec that:
- Describes the technical approach to satisfy the Plan
- Documents key design decisions with rationale
- Addresses testability (how will this be verified?)
- Identifies risks and dependencies

Respond with a JSON array containing a single `create_spec` action with `plan_id` set to "{plan.id}".
```

#### Phase Generation Prompt (user message, level-specific)

```
## Active Spec

**{spec.title}** (ID: {spec.id}, under Plan: {plan.title})
{spec.description}

## Relevant Learnings

{learnings scoped to this Spec + Plan + Global}

{if previous_validation_failures:}
## Previous Validation Failures (fix these)

{all accumulated validation issues}

## Instructions

Break this Spec into ordered implementation Phases. Each Phase should:
- Have a clear, actionable title
- Describe concrete deliverables
- Identify dependencies on other Phases
- Be implementable in 1-5 Works

Respond with a JSON array of `create_phase` actions with `spec_id` set to "{spec.id}", ordered by implementation sequence.
```

#### Work Generation Prompt (user message, level-specific)

```
## Active Phase

**{phase.title}** (Phase {phase.order}, ID: {phase.id}, of Spec: {spec.title})
{phase.description}

## Existing Works in this Phase

{list of existing Works with IDs and statuses, or "None yet"}

## Relevant Learnings

{learnings scoped to Phase + Spec + Plan + Global}

## Codebase Context

{researcher findings about affected modules}

## Instructions

Create Works for this Phase. Each Work should:
- Be small enough for an Implementer to complete in ~5-10 iterations
- Have a clear title and description with acceptance criteria
- Include resource_tags identifying affected files/modules
- Identify dependencies on other Works

Respond with a JSON array of `create_work` actions with `phase_id` set to "{phase.id}".
```

#### Researcher System Prompt

```
You are a Researcher agent in the Loopr development orchestrator. Your role is to
investigate the codebase and produce findings that help other agents make decisions.

## Your Query

{query from SpawnResearcher action}

## Your Capabilities

1. `search_code` — Search file contents with regex patterns
   {"action": "search_code", "pattern": "fn\\s+handle_", "glob": "*.rs", "path": "src/"}
2. `search_files` — Find files by glob pattern
   {"action": "search_files", "pattern": "**/*test*.rs", "path": "src/"}
3. `read_file` — Read a file's contents (relative to repo root)
   {"action": "read_file", "path": "src/agents/mod.rs"}
4. `list_directory` — List files in a directory
   {"action": "list_directory", "path": "src/agents"}
5. `create_learning` — Record a finding as a Learning
   {"action": "create_learning", "content": "...", "scope": "spec", "source_id": "...", "resource_tags": ["src/agents/"]}
6. `done` — Complete with a summary
   {"action": "done", "summary": "Found 3 relevant patterns..."}

## Rules

- You are read-only. You cannot modify any files.
- Focus on answering the specific query you were given.
- Create Learnings for significant findings that other agents should know.
- Be thorough but concise. Don't dump entire file contents into Learnings.
- Prioritize: patterns, conventions, dependencies, test coverage, API contracts.
- All file paths must be relative to the repo root.

## Output Format

Respond with ONLY a JSON array of actions.
```

**(No Integrator system prompt — the Integrator is deterministic code, not an LLM agent.)**

### Config Extensions

```rust
pub struct AgentConfig {
    // ... existing fields (enabled, auto_start_implementer, auto_start_reviewer,
    //                      implementer, reviewer, tools) ...
    pub coordinator: CoordinatorConfig,    // NEW
    pub researcher: AgentRoleConfig,       // NEW
    pub auto_start_coordinator: bool,      // NEW — default false
}

/// Coordinator-specific config extending AgentRoleConfig.
pub struct CoordinatorConfig {
    #[serde(flatten)]
    pub role: AgentRoleConfig,
    pub active_interval_secs: u64,       // default 5
    pub idle_interval_secs: u64,         // default 30
    pub max_validation_attempts: u32,    // default 3
}

// IntegratorConfig is extended (see Integrator Task section above), NOT part of AgentConfig.
// The Integrator is deterministic code, not an LLM agent.
```

Defaults (LLM agents only):

| Role | Model | Temperature | Max Tokens | Max Iterations | Pool Size | Extra |
|------|-------|-------------|------------|----------------|-----------|-------|
| Coordinator | claude-sonnet-4-6 | 0.2 | 8192 | ∞ (long-lived) | 1 | active_interval: 5s, idle_interval: 30s, max_validation_attempts: 3 |
| Researcher | claude-sonnet-4-6 | 0.1 | 4096 | 10 | 4 | — |
| Implementer | claude-sonnet-4-6 | 0.3 | 8192 | 20 | 2 | (unchanged) |
| Reviewer | claude-sonnet-4-6 | 0.1 | 4096 | 5 | 2 | (unchanged) |

Integrator defaults (no LLM): `interval_secs: 15`, `enabled: false`

**Coordinator pool_size = 1**: There is exactly one Coordinator. This is an intentional bottleneck — the single-authority principle. Multiple Coordinators would create conflicting decisions.

**Integrator pool_size = 1**: Same reasoning. One integration pipeline. Serial by design.

**Researcher pool_size = 4**: Researchers are cheap, parallel, read-only. The Coordinator can spawn multiple to investigate different questions simultaneously. Proposers share the Researcher pool.

### Strategy Knobs

Configurable policies that control system behavior. Each is an enum in config with a default:

```rust
/// Top-level Config gains a `strategy` field:
/// pub struct Config { ..., pub strategy: StrategyConfig }

pub struct StrategyConfig {
    pub stale_policy: StalePolicy,           // default: ReplanAtSafePoint
    pub conflict_policy: ConflictPolicy,     // default: LockAdvisory
    pub tick_cadence: TickCadence,           // default: Continuous
    pub bundle_size: BundleSizePolicy,       // default: { max_files: 8, max_loc: 300 }
    pub validator_strictness: ValidatorStrictness,  // default: HardFailOnAnyAmbiguity
    pub promotion: PromotionPolicy,          // default: { min_reinforcements: 3, max_age_days: 30, auto_promote: true }
    pub max_lock_ttl_minutes: u64,           // default: 60
}

pub enum TickCadence {
    Continuous,
    Batched { min_bundles: u32, timeout_secs: u64 },  // e.g., wait for 3 bundles or 300s
}
```

| Knob | Options | Default | Where Enforced |
|------|---------|---------|----------------|
| **Stale Policy** | `ReplanAtSafePoint` — agent rebases and re-tests at next safe point; `RejectIfStale` — Bundle rejected outright; `AutoReplayAndVerify` — daemon auto-rebases and re-runs validation | `ReplanAtSafePoint` | Integrator (at SealTick), Implementer (at ProposeBundle) |
| **Conflict Policy** | `LockAdvisory` — locks checked, conflicts detected at merge time; `LockStrict` — file writes to locked paths rejected by executor | `LockAdvisory` | Executor (WriteFile), Coordinator (AcquireLock) |
| **Tick Cadence** | `Batched` — Integrator waits for N Accepted Bundles or a timeout before creating a Tick; `Continuous` — Integrator creates a Tick as soon as any Bundle is Accepted | `Continuous` | Integrator task |
| **Bundle Size** | `max_files_touched: u32` (default 8), `max_loc_changed: u32` (default 300) | See defaults | Implementer prompt guidance + Coordinator Work sizing |
| **Validator Strictness** | `HardFailOnAnyAmbiguity` — any ambiguity in doc = Fail; `AllowAmbiguityWithFlags` — ambiguity = Warn, not Fail; `SuggestOnly` — all issues are Info, never Fail | `HardFailOnAnyAmbiguity` | Doc Validator prompt + validation gate logic |
| **Promotion** | `min_reinforcements: u32` (default 3), `max_age_days: u32` (default 30), `auto_promote: bool` (default true) | See defaults | Learning auto-promotion check (on reinforce) |

**Stale policy detail:** When a new Tick is published, the Implementer detects staleness (existing `drain_tick_published()` mechanism). With `ReplanAtSafePoint` (default), the Implementer rebases its worktree and re-runs tests before the next ProposeBundle. With `RejectIfStale`, the Integrator hard-rejects any Bundle whose `base_tick_id` doesn't match the latest Tick. With `AutoReplayAndVerify`, the Integrator auto-rebases the Bundle's branch and re-runs validation (more complex, but avoids wasted Implementer iterations).

**Learning auto-promotion:** When `auto_promote` is true, `Learning::reinforce()` checks if `reinforcements >= min_reinforcements` and age <= `max_age_days`. If both conditions are met AND `contradictions == 0`, the Learning is automatically promoted to Policy. This replaces the "human Coordinator promotes manually" approach — the system learns from consistent reinforcement.

**Contradiction after promotion:** If a promoted Learning (Policy) receives a contradiction, it is NOT auto-demoted. Instead, the Coordinator is notified via a DaemonEvent (`learning.policy_contradicted`) with the Learning ID and contradiction content. The Coordinator can then decide to demote, investigate (spawn a Researcher), or ignore. Policies are high-value and should not flip-flop automatically.

### Safety Guardrails

#### Pool Size Enforcement

`handle_agent_start` enforces pool_size as a **hard guard** before creating any session:

```rust
// Count active (non-terminal) sessions of the requested agent_type
let active_count = stores.agent_sessions.read().unwrap()
    .values()
    .filter(|s| s.agent_type == requested_type && !s.status.is_terminal())
    .count();
if active_count >= pool_size_for(requested_type, config) {
    return Err(error(-32002, format!("pool_size exceeded for {:?}", requested_type)));
}
```

This is a daemon-level guard. The LLM prompt says "don't exceed pool_size" as guidance, but the handler enforces it regardless. A buggy Coordinator cannot spawn unlimited agents.

Additionally, a **global agent cap** of 20 total active sessions prevents resource exhaustion from any combination of agent types.

#### Agent Session Timeout

Each agent session has a wall-clock timeout:

| Agent | Timeout |
|-------|---------|
| Coordinator | None (runs indefinitely on a timer) |
| Researcher | 10 minutes |
| Implementer | 30 minutes |
| Reviewer | 10 minutes |
| Integrator | 20 minutes |

The executor wraps `run_agent_loop()` in `tokio::time::timeout()`. On timeout, the session transitions to `Failed` with error "session timed out".

#### Agent Cancellation Check

At the start of each iteration, the agent loop re-reads its session status from the in-memory `Stores` HashMap. If the status has been set to `Cancelled` or `Failed` by an external action (human via TUI `agent.stop`, or another agent), the loop exits immediately. This prevents a "stopped" agent from continuing to run on a stale clone of its session.

#### Human Override Mechanism

The human always takes priority over the Coordinator:

1. **Pause/Resume:** TUI keybinding `p` pauses the Coordinator (`agent.pause`). Keybinding `r` resumes. While paused, the human can manually create/edit/transition hierarchy records.

2. **Manual edits respected:** When the Coordinator resumes (or starts a new iteration), it reads fresh state from TaskStore. Any records the human created, modified, or transitioned are immediately visible. The Coordinator does not "undo" human changes.

3. **Goal override:** The human can call `coordinator.set_goal` at any time to change the Coordinator's objective. The Coordinator picks up the new goal on its next iteration.

4. **Emergency stop:** `agent.stop` (TUI keybinding `x`) cancels the Coordinator entirely. It must be manually restarted.

### IPC Extensions

New methods added to the daemon's `dispatch()`:

```
coordinator.set_goal { goal: String }    — Set the Coordinator's plan-generation objective
coordinator.clear_goal {}                — Clear the goal (Coordinator operates on existing hierarchy only)
```

Most agent actions map to existing IPC methods (plan.create, spec.create, work.create, agent.start, bundle.transition, tick.create, etc.) via the `AgentIpcBridge`. The Coordinator, Researcher, and Integrator do NOT need custom IPC methods — they use the same `dispatch()` as everyone else.

### Implementation Plan

#### Phase 1: Context Builder + Learning Enrichment + Strategy Knobs (Foundation)

**What:** Generic `build_context()`, enriched Learning model with auto-promotion, learning selection, strategy knobs config.

**Files:**
- `src/agents/context.rs` (NEW) — `ContextBuilder`, `TokenBudget`, `AssembledContext`, `select_learnings()`
- `src/domain/learning.rs` (MODIFY) — Add `applicable_roles`, `resource_tags`, `confidence` fields with `#[serde(default)]`; `recompute_confidence()`; auto-promotion in `reinforce()`
- `src/domain/role.rs` (MODIFY) — Add `Role::Researcher`
- `src/config.rs` (MODIFY) — Add `StrategyConfig` with all strategy knobs (StalePolicy, ConflictPolicy, TickCadence, BundleSizePolicy, ValidatorStrictness, PromotionPolicy)
- `src/agents/implementer.rs` (MODIFY) — Replace `load_context()` + `build_user_message()` with `ContextBuilder`; add staleness handling per `StalePolicy`
- `src/agents/reviewer.rs` (MODIFY) — Replace `load_review_context()` + `build_review_message()` with `ContextBuilder`

**Tests:**
- Learning confidence computation (0 reinforcements, mixed, all reinforced, all contradicted, clamping)
- Learning backward compatibility (deserialize pre-MVP4 JSON, verify defaults)
- Learning auto-promotion (reinforce to threshold → auto-promoted; contradictions block promotion)
- Learning selection (scope filtering, role filtering, confidence threshold, promoted always included, deduplication, ordering)
- Context builder produces correct sections for Implementer role
- Context builder produces correct sections for Reviewer role
- Token budget enforcement (truncation at item boundaries, `[truncated]` marker)
- Strategy knobs: StalePolicy variants (ReplanAtSafePoint vs RejectIfStale behavior)
- Strategy knobs: ConflictPolicy variants (LockAdvisory vs LockStrict behavior)
- Strategy knobs: config deserialization from YAML

**Deliverable:** Existing Implementer and Reviewer agents work identically but use the new generic context builder. Strategy knobs configurable. Learning auto-promotion functional.

#### Phase 2: Coordinator Agent

**What:** The Coordinator agent loop, system prompt, actions, and executor integration.

**Files:**
- `src/agents/coordinator.rs` (NEW) — `CoordinatorContext`, `load_coordinator_context()`, `SYSTEM_PROMPT`, `build_coordinator_message()`, `parse_coordinator_actions()`, `run_coordinator()`
- `src/agents/mod.rs` (MODIFY) — Add `AgentType::Coordinator`, extend `AgentSession` with `target_id` and `query`, extend `AgentAction` with Coordinator variants, add `role` field to `Transition`
- `src/agents/executor.rs` (MODIFY) — Add Coordinator dispatch in `run_agent_loop()`, add `execute_action()` cases for all Coordinator actions, add role inference for `Transition`, add per-iteration cancellation check
- `src/config.rs` (MODIFY) — Add `CoordinatorConfig`, `coordinator` field in `AgentConfig`
- `src/daemon/handlers.rs` (MODIFY) — Handle `agent.start` for Coordinator type (no worktree, pool_size enforcement), add `coordinator.set_goal` / `coordinator.clear_goal` handlers
- `src/daemon/context.rs` (MODIFY) — Extend `recover_orphaned_records()` for stuck Ticks; add `CoordinatorGoal` record to TaskStore

**Tests:**
- Pool_size enforcement (reject second Coordinator start)
- Coordinator context loads all active hierarchy state including Draft records and active Locks
- Coordinator action parsing (all new action types, including JSON schema examples)
- Coordinator action execution (create plan → verify in TaskStore)
- AcquireLock / ReleaseLock (acquire on resource_tags, release on Work completion)
- Lock conflict: Coordinator rejects assignment when resource already locked by another Work
- AssignAgent with pool full → returns error
- AssignAgent success → agent session created, locks acquired, agent.start called
- SpawnResearcher dedup (reject if same scope_id already running)
- ValidateDocument → returns ActionResult::ValidationResult
- Coordinator iteration loop with mock LLM
- Coordinator interval enforcement (adaptive: 5s active, 30s idle)
- Coordinator auto-restart after failure
- Coordinator cancellation check (stops when externally cancelled)
- set_goal / clear_goal via IPC (goal persisted in TaskStore, survives daemon crash)
- Iteration record: Coordinator creates structured Learning per iteration

#### Phase 3: Document Generation Pipeline

**What:** Level-specific generation prompts and the generate → validate → iterate cycle.

**Files:**
- `src/agents/generation.rs` (NEW) — `PlanGenerationPrompt`, `SpecGenerationPrompt`, `PhaseGenerationPrompt`, `WorkGenerationPrompt`, `build_generation_message()`
- `src/agents/coordinator.rs` (MODIFY) — Add generation prompt selection based on current level; wire in validation loop with max_validation_attempts cap

**Tests:**
- Plan generation prompt includes user intent, learnings, and accumulated validation failures
- Spec generation prompt includes Plan context (with ID), findings, and accumulated failures
- Phase generation prompt produces ordered phases with spec_id
- Work generation prompt includes existing Works (avoids duplicates)
- Generate → validate → re-generate loop (mock validator returns Fail, then Pass)
- Generate → validate 3x fail → NeedHelp (verify max_validation_attempts cap)
- Accumulated failures: all previous issues included in re-generation context
- Integration test: full Plan → Spec → Phase → Work generation pipeline with mock LLM

#### Phase 4: Researcher Agent

**What:** Researcher agent loop with codebase search capabilities.

**Files:**
- `src/agents/researcher.rs` (NEW) — `ResearcherContext`, `SYSTEM_PROMPT`, `build_research_message()`, `parse_researcher_actions()`, `run_researcher()`, search execution
- `src/agents/mod.rs` (MODIFY) — Add `AgentType::Researcher`, add `SearchCode`/`SearchFiles`/`ListDirectory` to `AgentAction`
- `src/agents/executor.rs` (MODIFY) — Add Researcher dispatch and action cases, path sandboxing for all read operations
- `src/config.rs` (MODIFY) — Add `AgentRoleConfig::default_researcher()`

**Search implementation:** `SearchCode` wraps `Command::new("rg")` with pattern, glob filter, `--no-follow` flag, and path arguments. `SearchFiles` uses the `glob` crate (not shell `find`). Output truncated to 32KB (same as ToolRunner).

**Path sandboxing (applied to ReadFile, SearchCode, SearchFiles, ListDirectory):**
1. Reject absolute paths
2. Join to repo root
3. Canonicalize via `std::fs::canonicalize()`
4. Validate starts with repo root path
5. Check against denylist: `.env`, `*.key`, `*.pem`, `credentials.*`, `*secret*`

**Tests:**
- Researcher action parsing
- SearchCode execution (mock filesystem, verify regex matching)
- SearchFiles execution (verify glob matching via `glob` crate)
- ReadFile path sandboxing: reject `../../.env`, reject `/etc/passwd`, reject `.env`, reject symlink escape
- Output truncation at 32KB
- Researcher session timeout (10 minutes)
- Researcher iteration loop with mock LLM
- Researcher creates Learning from findings (with resource_tags)
- Researcher dedup: spawn rejected if same scope_id already running
- Integration: Coordinator spawns Researcher, picks up Learning on next iteration

#### Phase 5: Integrator Task (Deterministic)

**What:** Deterministic Integrator Tokio task automating Tick lifecycle. No LLM, no prompt, no parser.

**Files:**
- `src/agents/integrator_task.rs` (NEW) — `run_integrator()`, deterministic loop, merge logic, validation runner
- `src/agents/mod.rs` (MODIFY) — Add `AgentType::Integrator` (for session tracking only — not dispatched to LLM)
- `src/config.rs` (MODIFY) — Extend `IntegratorConfig` with `interval_secs` and `enabled`
- `src/daemon/handlers.rs` (MODIFY) — `agent.start` for Integrator spawns `run_integrator()` directly (no LLM client creation)
- `src/daemon/context.rs` (MODIFY) — Extend crash recovery for stuck Ticks (Sealing/Validating → Failed on startup)

**Key difference from LLM agents:** No `AgentLlmClient`, no system prompt, no action parsing. The Integrator uses `Role::Integrator` through the IPC bridge directly. All FSM guards apply. The staleness guard on Bundles is enforced per `StrategyConfig.stale_policy`. Tick cadence follows `StrategyConfig.tick_cadence` (Continuous or Batched).

**Validate-then-mutate for SealTick:** The handler validates all Bundle preconditions (Accepted status, base_tick_id matches latest Tick) before performing any mutations. If any check fails, no mutations occur. Only after all validations pass does it transition the Tick to Sealing and Bundles to Integrating.

**Tests:**
- Integrator singleton enforcement (reject second start)
- Crash recovery: stuck Ticks (Sealing/Validating from crash) → Failed on startup
- Full Tick lifecycle: create → seal → validate → publish (with mock validation commands)
- Validate-then-mutate: stale Bundle in batch → entire seal rejected, Tick stays Open
- Tick validation failure: create → seal → validate fails → fail Tick → reject Bundles → create Learning
- Stale Bundle rejection: Bundle with old base_tick_id rejected at seal time
- Integration: Bundle accepted → Integrator creates Tick → publishes → Implementer detects staleness
- Integrator respects cancellation (agent.stop)

#### Phase 6: TUI + CLI + Integration Tests

**What:** Wire everything into the user-facing interfaces.

**Files:**
- `src/tui/views/agents.rs` (MODIFY) — Show all 5 agent types in agent view
- `src/tui/input.rs` (MODIFY) — Add keybindings: `g` (set goal), `p` (pause Coordinator), `r` (resume), `x` (stop agent)
- `src/cli/dispatch.rs` (MODIFY) — Add CLI commands: `loopr coordinator set-goal`, `loopr coordinator clear-goal`, `loopr agent start coordinator`, etc.
- `src/daemon/handlers.rs` (MODIFY) — Ensure `agent.start` handles all 5 types, pool_size + global cap enforcement

**Integration tests (end-to-end):**
- Full pipeline: human sets goal → Coordinator creates Plan → validates → generates Spec → validates → generates Phases → validates → creates Works → assigns Implementer → Implementer proposes Bundle → Coordinator triages + routes to Reviewer → Reviewer approves → Coordinator accepts → Integrator creates Tick → publishes → Coordinator marks Phase complete
- Human override: pause Coordinator, manually edit Plan, resume → Coordinator respects changes
- Coordinator recovery: daemon restart, Coordinator session persists in TaskStore, auto-restarts
- Tick crash recovery: daemon restart with stuck Tick in Validating → Tick marked Failed → Integrator proceeds
- Learning propagation: Implementer creates Learning → Coordinator picks it up → influences next Work
- Multi-agent coordination: Coordinator + 2 Implementers + 1 Reviewer + 1 Integrator + 2 Researchers running concurrently
- Pool exhaustion: Coordinator tries to spawn beyond pool_size → gets error → handles gracefully

**Phase dependencies:**

```
Phase 1 (Foundation)
  ├── Phase 2 (Coordinator) ── Phase 3 (Generation Pipeline)
  ├── Phase 4 (Researcher)
  └── Phase 5 (Integrator)
All ────────────────────────── Phase 6 (TUI + CLI + Integration)
```

Phases 2, 4, and 5 have no semantic dependencies on each other after Phase 1. However, all three modify the same files (`mod.rs`, `executor.rs`, `config.rs`, `handlers.rs`), so parallel development will cause merge conflicts. **Recommended approach:** Phase 1 establishes the extension points (per-role action sub-enums, trait-based executor dispatch) so that Phases 2/4/5 primarily add new files rather than modifying shared ones. If developed by a single developer (likely for MVP4), sequence them: Phase 2 → Phase 4 → Phase 5, with Phase 3 after Phase 2. Phase 6 depends on all.

## Alternatives Considered

### Alternative 1: Coordinator as External Process (Claude Code Session)

**Description:** Instead of a Tokio task inside the daemon, run the Coordinator as a Claude Code session (like `bin/loop.sh`) that communicates via the Unix socket IPC.

**Pros:** Leverages Claude Code's built-in tools (file editing, terminal, web search). More capable out of the box.

**Cons:** Multi-writer risk — Claude Code sessions can modify files outside the daemon's control. Loses the single-authority guarantee that is the entire point of the architecture. Can't enforce FSM transitions. The exact problem Gas Town has.

**Why not chosen:** The daemon-as-single-authority is the core architectural invariant. Every state mutation must go through the daemon's FSM validation. An external process would bypass this.

### Alternative 2: Full Swarm Architecture for Document Generation

**Description:** Instead of the Coordinator generating documents itself, spawn 5-20 lightweight proposal tasks for every document generation step.

**Pros:** Higher quality through diversity at every level.

**Cons:** 5-20x more LLM calls per generation step. Requires ranking/selection for every document.

**Why not chosen in full:** MVP4 includes a lightweight proposer mechanism (2-3 Proposers, using the Researcher pool) for complex Specs and Phases. The full 5-20 swarm per level is an optimization that can scale up from this foundation. The Coordinator decides when to use direct generation (simple) vs. proposer spawning (complex) — this is more efficient than always swarming.

### Alternative 3: Merge Researcher into Coordinator

**Description:** Instead of a separate Researcher agent, give the Coordinator search capabilities directly.

**Pros:** Simpler. Fewer agent types. No inter-agent coordination needed.

**Cons:** Bloats the Coordinator's action set and context. Research can take many iterations (searching, reading, summarizing) which would consume the Coordinator's iteration budget. The Coordinator should be making decisions, not reading files.

**Why not chosen:** Separation of concerns. The Coordinator decides what needs investigating. The Researcher investigates. The Coordinator picks up findings as Learnings. This keeps each agent focused and its context window clean.

### Alternative 4: New Record Types (Proposal, Decision, Finding)

**Description:** Add `Proposal`, `Decision`, and `Finding` as new TaskStore record types, as discussed in the ChatGPT conversation.

**Pros:** Richer domain model. More explicit tracking of the decision process.

**Cons:** More types to maintain, serialize, display in TUI. The existing hierarchy records (Plan, Spec, Phase as Draft) already serve as proposals. The transition Draft → Active already serves as a decision. Learnings already serve as findings.

**Why not chosen:** Use existing types. A Draft Plan IS a proposal. Transitioning to Active IS a decision. A Learning IS a finding. New types add complexity without adding capability in MVP4. Can be revisited if the model proves insufficient.

## Technical Considerations

### Dependencies

- `glob` crate — for Researcher's `SearchFiles` action (glob pattern matching). Add via `cargo add glob`.
- No other new external crates required. All other search operations use `Command::new("rg")`.
- The existing `AgentLlmClient` (reqwest, async, streaming) serves all new agents. No changes to the LLM client.
- The existing `AgentIpcBridge` serves all new agents. The bridge already wraps `handlers::dispatch()`.

### Performance

- Coordinator runs on adaptive timer (5s active, 30s idle). During active work: ~12 LLM calls per minute maximum. During idle: ~2 per minute.
- Researcher searches are bounded by the 32KB output truncation, 10-iteration cap, and 10-minute timeout.
- Integrator runs validation commands sequentially (same as current manual integrator).
- Context builder token estimation uses word-count heuristic (~1.3 tokens per word, ~4 characters per token). Not exact, but sufficient for budgeting.
- **Learning store growth:** Coordinator does NOT persist "iteration summary" Learnings every iteration — only genuine process insights. GC and cap deferred to MVP5; MVP4 monitors growth and flags if manual cleanup is needed.
- **Integrator has zero LLM cost.** It is deterministic code — no API calls, no token consumption, no parsing latency.

### Security

- Researcher is read-only. Cannot write files, run arbitrary commands, or modify state (except creating Learnings).
- **Researcher path sandboxing:** All file operations validate paths are within repo root after canonicalization. Absolute paths rejected. Symlink following disabled (`--no-follow`). Denylist blocks `.env`, `*.key`, `*.pem`, `credentials.*`, `*secret*`.
- Coordinator creates records through the IPC bridge with `Role::Coordinator`. FSM guards enforce valid transitions.
- Integrator uses `Role::Integrator` via the `role` field on `Transition`. Cannot make Coordinator-only transitions.
- `ValidateTick` runs `IntegratorConfig.validation_commands`. The tick_id is used only as a lookup key, never interpolated into shell commands. Validation commands run with a timeout (same as ToolRunner).
- No new network access. All LLM calls go through the existing `AgentLlmClient`.

### Concurrency

**Lock snapshot pattern:** Context builders do NOT hold read locks across the entire assembly. They acquire each lock briefly, clone needed records, release the lock, then format strings and count tokens on local data. This prevents blocking handlers that need write locks. See the Context Builder section for the pattern.

**Lock ordering invariant (for handlers):** Code paths that must acquire multiple locks follow this order: `plans → specs → phases → works → bundles → ticks → learnings → locks → agent_sessions → Store mutex`. Handlers acquire at most one write lock at a time, release it, then acquire the Store mutex for persistence.

**Lock poisoning:** All `.unwrap()` calls on lock acquisitions are replaced with `.unwrap_or_else(|e| e.into_inner())` to recover from poisoned locks (a thread panic while holding a lock). Alternatively, consider migrating to `parking_lot::RwLock` which does not have lock poisoning.

### Testing Strategy

**Unit tests (per-module, with mock LLM):**
- Context builder: section assembly, token budgeting, learning selection, truncation
- Learning enrichment: confidence computation, clamping, role filtering, age decay, backward compat
- Each agent: action parsing, context loading, iteration loop
- Action execution: each new action type via mock IPC bridge
- Pool_size enforcement: hard guard rejects over-limit spawns
- Path sandboxing: reject all escape attempts

**Integration tests (multi-agent, with mock LLM):**
- Full pipeline: Plan → Spec → Phase → Work → Bundle → Tick
- Agent coordination: Coordinator spawns Implementer, waits for completion
- Learning propagation: agent creates Learning, another agent picks it up
- Failure recovery: agent fails, session persists, next iteration recovers
- Crash recovery: stuck Ticks recovered on restart

**LLM testing (the hard part):**

The generate → validate → iterate cycle is where LLM quality matters most. Testing strategy:

1. **Deterministic mock:** Mock LLM returns canned responses. Tests verify the orchestration logic (action parsing, execution, state transitions) independent of LLM quality.

2. **Golden file tests:** For each generation prompt type, maintain a set of golden inputs (Plan context, Spec context, etc.) and golden outputs (expected Plan, Spec, Phase records). Run the real LLM against golden inputs and compare outputs against golden expectations. These are slow, expensive, and non-deterministic — run as a separate CI stage, not on every commit.

3. **Validator as quality gate:** The Doc Validator serves as the runtime quality check. If the Coordinator generates a bad Spec, the validator catches it and the Coordinator re-generates with feedback. This is the self-correcting loop. The test for "does generation work?" is ultimately "does it pass validation within N attempts?"

4. **Structured output validation:** Regardless of LLM quality, verify that all agent responses parse correctly into the expected action types. This catches format drift.

### Rollout Plan

1. Merge Phase 1 (Context Builder) first. Existing agents use it. Verify no regressions with `otto ci`.
2. Merge Phase 2 (Coordinator) with `auto_start_coordinator: false`. Manually start via TUI/CLI for testing.
3. Merge Phases 3-5 incrementally, each tested independently.
4. Phase 6 enables TUI integration and adds end-to-end integration tests.
5. Default config remains `agents.enabled = false`. Users opt in per-agent via `auto_start_*` flags.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Coordinator generates low-quality Plans/Specs | High | Medium | Doc Validator gates quality. Generate-validate-iterate loop self-corrects. Human can pause and override via TUI. |
| Coordinator enters infinite generate-validate loop | Medium | High | `max_validation_attempts` (default 3). After cap, `NeedHelp` pauses Coordinator. All previous failures included in re-gen context to prevent oscillation. |
| Coordinator thrashes creating duplicate Drafts | Medium | High | Draft-awareness rule: check for existing Drafts before generating. Iterate on existing Drafts instead of creating new ones. |
| Coordinator and human conflict on state | Medium | Medium | Human always wins. Pause-on-override. Coordinator reads fresh state each iteration. Manual edits respected. |
| Researcher returns too much/too little context | Medium | Low | 32KB output truncation. 10-iteration cap. 10-minute timeout. Coordinator can spawn multiple Researchers with refined queries. |
| Researcher reads sensitive files | Low | High | Path sandboxing, denylist, symlink prevention, absolute path rejection. |
| Agent pool exhaustion | Medium | High | Hard pool_size guard in `handle_agent_start`. Global cap of 20 sessions. LLM prompt guidance is belt; handler guard is suspenders. |
| Orphaned agent (hung LLM, infinite loop) | Medium | Medium | Per-session wall-clock timeout. Coordinator context includes active sessions so it can detect orphans. |
| Integrator crashes mid-Tick | Low | High | Crash recovery: stuck Ticks (Sealing/Validating) → Failed on daemon restart. Integrator also checks at iteration start. |
| Integrator publishes bad Tick | Low | High | Validation commands are configurable. User sets the CI pipeline they trust. Same `IntegratorConfig` as manual integration. Deterministic code means no LLM parsing failures in the integration pipeline. |
| Context builder token estimates are inaccurate | Medium | Low | Estimates for budgeting only. LLM handles overflow. Truncation at item/sentence boundaries. |
| Learning store grows unbounded | Medium | Medium | No per-iteration summaries. GC deferred to MVP5; monitor growth and flag for manual cleanup. |
| Learning enrichment migration breaks existing data | Low | Medium | All new fields use `#[serde(default)]`. Migration test verifies backward compat. |
| Lock contention with 10+ agents | Medium | Low | Consistent lock ordering. Single write lock at a time. Read-heavy workload. Consider `parking_lot` if contention measured. |
| Agent session state divergence (clone vs live) | Medium | High | Per-iteration cancellation check reads live session status from HashMap. |

## Open Questions

- [ ] Should Researcher be able to search Git history (`git log`, `git blame`), or just current working tree?
- [ ] Should Learning GC (deferred to MVP5) run as a periodic daemon task, or only when the cap is hit?
- [ ] Should the `CoordinatorGoal` record support multiple queued goals, or only one active goal at a time?

## References

- `docs/design/2026-02-25-orchestration-spine.md` — Orchestration spine
- `docs/design/2026-02-26-taskstore-doc-validator.md` — TaskStore + Doc Validator
- `docs/design/2026-02-26-implementer-reviewer-agents.md` — Implementer + Reviewer agents
- `docs/v3-chatgpt-loopr-architecture-conversation.md` — Original vision (6 personas, swarms, multi-level RWL)
- `docs/v3-claude-loopr-mvp-and-fsm-conversation.md` — FSM decisions, MVP phasing
- `docs/v3-preplan-conversation.md` — IPC vs TaskStore-as-bus, daemon justification
