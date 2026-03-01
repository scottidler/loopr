# Design Document: Agent Struct Refactoring

**Author:** Scott Idler
**Date:** 2026-03-01
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

Refactor all five agent modules (Coordinator, Implementer, Reviewer, Researcher, Integrator) from free functions with 6-8 threaded parameters to proper structs implementing a common `Agent` trait. This eliminates duplicated boilerplate, enables `self.ctx.log.info()` ergonomics, makes executor dispatch polymorphic, and provides a foundation for shared agent lifecycle behavior.

## Problem Statement

### Background

Loopr's agents were built incrementally across MVPs 1-5. Each started as a single `run_*()` function and grew helpers as complexity increased. The codebase compensated for parameter explosion by introducing ad-hoc context structs (`IterationContext` in coordinator.rs:846, `IterationParams` in implementer.rs:133) — these are agent structs in denial.

### Problem

The free-function architecture creates five concrete problems:

**1. Parameter threading explosion.** Every function in every agent takes the same 6-8 parameters. Adding a new shared resource (as we just did with `agent_log`) requires touching every function signature and every call site across every agent — the logging refactor required ~130 test site updates.

**2. Duplicated boilerplate in executor.rs dispatch.** `run_agent_loop` (executor.rs:310-468) has five match arms that each: clone config, create LLM client, clone session from stores, call the agent, write back iteration count. The session-clone and iteration-writeback blocks are copy-pasted verbatim across all five arms:

```rust
// This exact block appears 5 times (executor.rs:336, 371, 396, 422, 447)
let mut session = {
    let sessions = stores.agent_sessions.read().unwrap();
    sessions
        .get(session_id)
        .ok_or_else(|| eyre!("session not found: {}", session_id))?
        .clone()
};
```

**3. Inconsistent shared behavior.** `is_session_cancelled()` exists as a private function in both coordinator.rs:238 (2 params) and integrator.rs:23 (3 params) with different signatures. Iteration persistence happens in-loop for Coordinator and Implementer but in executor.rs for Researcher and Integrator. No single place enforces the contract.

**4. Testing verbosity.** Every test must construct 6-8 parameters independently. A test for a single helper function requires setting up stores, bridge, event_tx, agent_log, config — even if the function only uses one of them.

**5. No polymorphic dispatch.** Adding a new agent type requires modifying `run_agent_loop` with a new match arm that follows the exact same template. The type system doesn't enforce the contract — any signature mismatch is a runtime surprise.

### Goals

- Agent structs with methods: `self.ctx.log.info()`, `self.run_iteration()`, etc.
- Common `Agent` trait with `run()` for polymorphic dispatch
- Shared `AgentContext` struct for cross-cutting fields (stores, bridge, event_tx, log)
- Delete `IterationContext` and `IterationParams` — the agent IS the context
- Deduplicate session-clone, iteration-writeback, cancellation-check patterns
- Each phase independently compilable and testable

### Non-Goals

- Changing AgentLogger internals or log format (just completed)
- Changing the LlmClient trait
- Changing AgentIpcBridge or Stores architecture
- Adding new agent capabilities or behaviors
- Changing the FSM state machine logic in any agent

## Proposed Solution

### Overview

Introduce an `Agent` trait and `AgentContext` shared struct in `src/agents/mod.rs`. Convert each agent from free functions to a struct embedding `AgentContext` + agent-specific fields. Migrate one agent at a time, simplest first.

### The `Agent` Trait

```rust
// src/agents/mod.rs

#[async_trait]
pub trait Agent: Send {
    /// Run the agent's main loop to completion.
    async fn run(&mut self) -> Result<()>;

    /// Agent type for dispatch and logging.
    fn agent_type(&self) -> AgentType;
}
```

Minimal — just `run()` and `agent_type()`. No default methods in the first pass. Each agent implements its own loop internally. Default methods for shared behavior are Phase 7.

### `AgentContext` — Shared Fields

```rust
// src/agents/mod.rs

pub struct AgentContext {
    pub session: AgentSession,
    pub stores: Arc<Stores>,
    pub bridge: AgentIpcBridge,       // Owned, not Clone (has AtomicU64)
    pub event_tx: broadcast::Sender<DaemonEvent>,
    pub tool_runner: Arc<ToolRunner>,  // Cloned from Stores (cheap Arc clone)
    pub log: AgentLogger,
}

impl AgentContext {
    /// Convenience logging delegates
    pub fn info(&self, msg: &str) { self.log.info(msg) }
    pub fn warn(&self, msg: &str) { self.log.warn(msg) }
    pub fn debug(&self, msg: &str) { self.log.debug(msg) }
    pub fn error(&self, msg: &str) { self.log.error(msg) }

    /// Check if this agent's session has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        let sessions = self.stores.agent_sessions.read().unwrap();
        sessions
            .get(&self.session.id)
            .map(|s| s.status == AgentStatus::Cancelled)
            .unwrap_or(true)
    }

    /// Persist current iteration count to stores.
    pub fn persist_iteration(&self) {
        let mut sessions = self.stores.agent_sessions.write().unwrap();
        if let Some(s) = sessions.get_mut(&self.session.id) {
            s.iteration = self.session.iteration;
        }
    }

    /// Emit an iteration-completed event.
    pub fn emit_iteration_completed(&self, iteration: u32, summary: &str) {
        let _ = self.event_tx.send(
            DaemonEvent::agent_iteration_completed(&self.session.id, iteration, summary)
        );
    }
}
```

### Per-Agent Structs

| Struct | Fields beyond `AgentContext` |
|--------|----------------------------|
| `IntegratorAgent` | `config: IntegratorConfig` |
| `ReviewerAgent` | `llm: Box<dyn LlmClient>`, `config: AgentRoleConfig`, `bundle_id: String` |
| `ResearcherAgent` | `llm: Box<dyn LlmClient>`, `config: AgentRoleConfig`, `previous_summary: Option<String>` |
| `ImplementerAgent` | `llm: Box<dyn LlmClient>`, `config: AgentRoleConfig`, `work_id: String`, `worktree_path: PathBuf`, `previous_summary: Option<String>`, `has_proposed: bool` |
| `CoordinatorAgent` | `llm: Box<dyn LlmClient>`, `config: CoordinatorConfig`, `coord_state: Option<CoordinatorState>`, `iteration: u32`, `previous_summary: Option<String>` |

Example struct:

```rust
// src/agents/integrator.rs

pub struct IntegratorAgent {
    pub ctx: AgentContext,
    config: IntegratorConfig,
}

impl IntegratorAgent {
    pub fn new(ctx: AgentContext, config: IntegratorConfig) -> Self {
        Self { ctx, config }
    }
}

#[async_trait]
impl Agent for IntegratorAgent {
    async fn run(&mut self) -> Result<()> {
        self.ctx.info(&format!("started (interval: {}s)", self.config.interval_secs));
        let interval = Duration::from_secs(self.config.interval_secs);

        loop {
            if self.ctx.is_cancelled() {
                self.ctx.info("cancelled, exiting loop");
                return Ok(());
            }
            self.ctx.session.iteration = self.ctx.session.iteration.saturating_add(1);

            match self.run_cycle() {
                // ... same logic, now as methods
            }

            tokio::time::sleep(interval).await;
        }
    }

    fn agent_type(&self) -> AgentType { AgentType::Integrator }
}
```

### `execute_action` — Stays Shared

`execute_action` is used by Coordinator, Implementer, and Researcher with different worktree paths and work_ids. It stays as a standalone async function but takes `&AgentContext` instead of individual params — **7 params → 4 params**:

```rust
// src/agents/executor.rs

pub async fn execute_action(
    action: &AgentAction,
    ctx: &AgentContext,          // provides tool_runner, bridge, log, agent_type
    worktree_path: &Path,
    work_id: Option<&str>,
) -> Result<ActionResult>
```

This replaces the current 7-parameter signature: `(action, tool_runner, bridge, worktree_path, work_id, agent_type, agent_log)`. The `tool_runner`, `bridge`, `agent_type` (via `ctx.session.agent_type`), and `agent_log` are all available via `ctx`.

### Executor Dispatch — Factory + Trait

```rust
// src/agents/executor.rs

fn create_agent(
    agent_type: AgentType,
    ctx: AgentContext,
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
) -> Result<Box<dyn Agent>> {
    match agent_type {
        AgentType::Integrator => {
            let config = stores.config.integrator.clone();
            Ok(Box::new(IntegratorAgent::new(ctx, config)))
        }
        AgentType::Reviewer => {
            let config = stores.config.agents.reviewer.clone();
            let llm = create_llm_client(&config, &ctx.session.id, event_tx)?;
            Ok(Box::new(ReviewerAgent::new(ctx, llm, config)))
        }
        // ... etc
    }
}

// In run_agent_task, replaces run_agent_loop:
let mut agent = create_agent(agent_type, ctx, &stores, &event_tx)?;
agent.run().await
```

The five repetitive match arms with copy-pasted session-clone and iteration-writeback collapse into a single `create_agent` factory + `agent.run()` call.

### Implementation Plan

Each phase is independently compilable and testable. `otto ci` must pass after each.

#### Phase 1: `AgentContext` + `Agent` trait + `IntegratorAgent`

Start with the simplest agent (no LLM, no worktree). Introduce the core types.

- Add `AgentContext` struct and `Agent` trait to `src/agents/mod.rs`
- Convert `run_integrator` + helpers → `IntegratorAgent` struct with `impl Agent`
- Helper functions (`is_session_cancelled`, `run_integrator_cycle`, `recover_stuck_ticks`, etc.) become private methods on `IntegratorAgent`
- Update executor.rs: Integrator arm uses `IntegratorAgent::new().run()`; other arms unchanged
- Update integrator tests to construct `IntegratorAgent` instead of calling free functions

**Files:** `src/agents/mod.rs`, `src/agents/integrator.rs`, `src/agents/executor.rs`

#### Phase 2: `ReviewerAgent`

Simplest LLM agent (single iteration, no loop). Validates the pattern works with LLM-backed agents.

- Convert `run_reviewer` + `parse_review_result` → `ReviewerAgent` methods
- Update executor.rs: Reviewer arm uses `ReviewerAgent::new().run()`

**Files:** `src/agents/reviewer.rs`, `src/agents/executor.rs`

#### Phase 3: `ResearcherAgent`

Multi-iteration LLM agent with researcher-specific actions (SearchCode, SearchFiles, etc.).

- Convert `run_researcher` + `run_researcher_iteration` + search helpers → `ResearcherAgent` methods
- `validate_path`, `execute_search_code`, `execute_search_files`, `execute_list_directory` become methods

**Files:** `src/agents/researcher.rs`, `src/agents/executor.rs`

#### Phase 4: `ImplementerAgent`

Most complex action agent. Delete `IterationParams`.

- Convert `run_implementer` + `run_iteration` + `parse_actions` + `build_implementer_summary` + `drain_tick_published` → methods
- `IterationParams` struct deleted — its fields are now on `ImplementerAgent`
- Force-propose logic at iteration cap becomes a method

**Files:** `src/agents/implementer.rs`, `src/agents/executor.rs`

#### Phase 5: `CoordinatorAgent`

Most complex overall. Delete `IterationContext`. FSM state management becomes struct methods.

- Convert `run_coordinator` + `run_coordinator_iteration` + `run_coordinator_legacy` → methods
- `IterationContext` deleted — its fields are on `CoordinatorAgent`
- `build_state_summary`, `build_fsm_footer`, `build_generation_footer`, `check_fsm_transition`, `mark_phase_record_complete`, `resolve_batch_dependencies` → private methods
- Coordinator restart loop (currently in executor.rs) moves into `CoordinatorAgent::run()`

**Files:** `src/agents/coordinator.rs`, `src/agents/executor.rs`

#### Phase 6: Refactor `execute_action` signature

Once all agents use `AgentContext`, simplify `execute_action`:

- Replace `(action, tool_runner, bridge, worktree_path, work_id, agent_type, agent_log)` → `(action, ctx, worktree_path, work_id)`
- Update all callers in agent methods

**Files:** `src/agents/executor.rs`, all agent files

#### Phase 7: Extract shared behavior into `AgentContext`

Move duplicated patterns into `AgentContext` methods or `Agent` trait defaults:

- `is_cancelled()` — replaces two private `is_session_cancelled` functions
- `persist_iteration()` — replaces 5 identical writeback blocks in executor.rs
- `emit_iteration_completed()` — standardizes emission across agents
- Session clone at startup — moves into `AgentContext::from_session_id()`

**Files:** `src/agents/mod.rs`, all agent files

## Alternatives Considered

### Alternative 1: Keep Free Functions, Add Helper Module

- **Description:** Extract shared patterns (`is_session_cancelled`, iteration writeback) into a `src/agents/common.rs` helper module. Keep free function architecture.
- **Pros:** Minimal change. No structural refactoring.
- **Cons:** Parameter threading persists. No `self` ergonomics. Executor dispatch stays repetitive. Adding new shared behavior still requires threading another parameter. Doesn't fix the root cause — just patches symptoms.
- **Why not chosen:** Doesn't address the core problem. We already did this with `agent_log` and it required touching ~130 call sites. Next time would be the same.

### Alternative 2: Single `Agent<C: AgentConfig>` Generic Struct

- **Description:** One generic `Agent<C>` struct parameterized by config type, with behavior differences via trait bounds or enum dispatch.
- **Pros:** Maximum code reuse. Single struct definition.
- **Cons:** Agents are genuinely different (Integrator has no LLM; Coordinator has FSM state; Implementer has worktree). Forcing them into a single generic creates awkward `Option` fields and runtime checks. Generic monomorphization bloats compile times.
- **Why not chosen:** The agents share infrastructure but differ in domain logic. Composition (shared `AgentContext` + per-agent struct) models this better than generics.

### Alternative 3: ECS-style Component Architecture

- **Description:** Each agent is an entity with components (LlmComponent, WorktreeComponent, FsmComponent). Behavior emerges from component composition.
- **Pros:** Maximum flexibility. Easy to add/remove capabilities.
- **Cons:** Massive over-engineering for 5 agents. Rust's type system makes ECS awkward without a framework. Runtime component lookup vs compile-time struct access.
- **Why not chosen:** We have 5 agents, not 500. YAGNI.

## Technical Considerations

### Dependencies

- `async_trait` crate (already a dependency) for `#[async_trait]` on the `Agent` trait
- No new external dependencies

### Performance

No performance impact. This is a compile-time structural change. The runtime behavior is identical — same functions called in the same order with the same data. Struct method dispatch is monomorphized (zero-cost) except for the single `Box<dyn Agent>` dispatch in executor.rs, which is one vtable lookup per agent lifetime.

### Testing Strategy

- Each phase must pass `otto ci` (compile + clippy + fmt + tests)
- No behavioral changes — tests verify identical outcomes
- `AgentContext` gets a `new_for_test()` factory that replaces the per-file `test_agent_logger` + `test_bridge` + `setup_stores` boilerplate
- Existing test helpers are reused to construct the struct
- Phase 7 can add `AgentContext`-level tests for shared behavior

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Borrow checker fights with `&mut self` + `&self.ctx.stores` | Medium | Medium | `stores` is `Arc<Stores>` with `RwLock` fields — interior mutability already works. The real risk is methods that need `&mut self.ctx.session` while also reading `&self.ctx.stores` — solved by destructuring `let AgentContext { ref mut session, ref stores, .. } = self.ctx;` or by cloning the Arc. |
| Large diff per phase makes review hard | Medium | Low | Each phase is one agent. Commit per phase. Tests enforce no behavioral change. |
| `ToolRunner` lifetime in `ImplementerAgent` | Medium | Low | `ToolRunner` is already `Arc`-wrapped in `Stores` (daemon/context.rs:52). Store `Arc<ToolRunner>` in the struct. |
| Coordinator restart loop (currently in executor.rs) needs access to create a fresh agent | Low | Medium | Move restart logic into `CoordinatorAgent::run()` as a self-restart. |

## Open Questions

- [x] Should `AgentContext` own the `ToolRunner` reference? **Yes** — `ToolRunner` is `Arc<ToolRunner>` in Stores (daemon/context.rs:52). Cloning the Arc into `AgentContext` is cheap. All three action agents (Coordinator, Implementer, Researcher) need it for `execute_action`. Keeping it in `AgentContext` avoids special-casing.
- [x] Should the Coordinator restart loop move into `CoordinatorAgent::run()`? **Yes** — it's Coordinator-specific behavior (no other agent retries). Moving it into the struct keeps executor.rs generic: just `agent.run().await`.

## References

- Prior MVP design docs: `docs/design/2026-02-25-loopr-v3-mvp1.md` through `mvp8`
- Logging refactor (just completed): commit `eb19488` — demonstrates the parameter-threading pain this refactor addresses
- `src/agents/executor.rs` — current dispatch layer (lines 310-468: `run_agent_loop`)
- `src/agents/mod.rs` — current `AgentType`, `AgentStatus`, `AgentSession` definitions
