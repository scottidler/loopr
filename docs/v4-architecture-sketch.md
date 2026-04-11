# Loopr v4 Architecture Sketch

Captured 2026-04-11. Pre-design-doc thinking on the v3→v4 rewrite.

## Motivation

v3's orchestration logic is procedural Rust — the coordinator, FSMs, recovery strategies,
decomposer, and agent wiring are all hardcoded. To add a new behavior (e.g., "ask a friend"
when an implementer gets stuck), someone must write Rust code, add FSM transitions, wire
handlers. This is a code change for what is fundamentally a policy decision.

v4 inverts the relationship: **Rust becomes the runtime, YAML defines the orchestration.**
The Rust code provides primitives (spawn agent, inject context, retry, escalate, abandon,
merge, etc.), an FSM interpreter, and a composition engine. YAML defines FSM shapes,
transition guards, recovery strategies, agent wiring, and decomposition pipelines. New
behaviors are YAML changes, not Rust changes.

The otto-rs/otto connection: otto lets users define build tasks, dependencies, and execution
strategies in YAML — the Rust runtime interprets and executes them. v4 applies the same
pattern to agent orchestration.

The AR connection: AutoResearch isn't just sweeping numeric parameters — it can compose
entirely new strategies and score them. "What if we add an advisor step after 2 failures?"
becomes a YAML change that AR can generate, run, and score — no human writing Rust.

## What v3 Teaches Us

v3 already answers the hard questions:
- What are the domain types
- What FSM states exist
- What primitives does the coordinator actually perform
- What tools do agents need
- What does the agentic loop look like

We don't have to discover any of that. We reorganize where those answers live — pull behavior
out of Rust into YAML, leave the runtime engine in Rust.

## Keep vs Rewrite

### Carry over directly (proven, architecture-neutral)

- Domain types (Plan, Spec, Phase, Work, Bundle, Session, Tick, Lock, Chat)
- TaskStore integration (JSONL-as-truth, the Record trait)
- IPC protocol (Unix socket, framing, client/server)
- TUI (ratatui views, event loop)
- Tools (Tool trait, all builtins)
- LLM client + streaming SSE
- Worktree management
- CLI (clap structure, dispatch)
- Scorer

### Rewrite from scratch (the orchestration spine)

- Coordinator → becomes a generic strategy interpreter
- Executor → becomes primitive-sequencing driven by strategy YAML
- Decomposer → becomes a pluggable strategy, not a hardcoded 4-level pipeline
- Supervisor → configurable restart policy from YAML
- Recovery logic → composable strategies
- Work queue → pluggable priority formula
- Daemon handlers → thin dispatchers into the engine

### Build new (doesn't exist in v3)

- Engine (composition interpreter, trigger evaluator, guard system)
- FSM runtime interpreter (replaces compile-time `#[derive(Fsm)]`)
- Primitive registry (formalized atomic operations)
- YAML schema for everything

## Proposed Skeleton

```
src/
  main.rs                    # thin shell (v3)
  lib.rs                     # crate root

  # --- THE ENGINE (new) ---
  engine/
    mod.rs                   # composition engine: loads strategies, runs them
    interpreter.rs           # evaluates trigger -> action -> wiring chains
    registry.rs              # primitive registry (name -> fn)

  fsm/
    mod.rs                   # generic FSM interpreter
    schema.rs                # parses+validates FSM definitions from YAML
    runtime.rs               # runtime FSM instance: state, transitions, guards

  trigger/
    mod.rs                   # trigger trait + evaluation
    threshold.rs             # consecutive-failures, attempt-count, ratio
    event.rs                 # FSM transition, agent event, timer
    composite.rs             # and/or/not combinators

  # --- PRIMITIVES (extracted from v3 coordinator) ---
  primitive/
    mod.rs                   # Primitive trait + registry
    agent.rs                 # spawn, stop, inject-context, ask-friend
    work.rs                  # retry, abandon, block, reassign, reprioritize
    decompose.rs             # decompose, re-decompose, promote, bubble-up
    integrate.rs             # merge, rebase, resolve-conflict
    escalate.rs              # need-help, surface-error, notify
    evaluate.rs              # validate, score, gate-check
    context.rs               # build-context, compact, inject-learning

  # --- DOMAIN (carry from v3) ---
  domain/
    mod.rs
    plan.rs, spec.rs, phase.rs, work.rs
    bundle.rs, session.rs, tick.rs, lock.rs, chat.rs

  # --- AGENTS (simplified - role-agnostic runtime) ---
  agents/
    mod.rs                   # generic agent runtime
    session.rs               # session lifecycle
    pool.rs                  # worker pool
    context.rs               # context builder (v3)
    llm.rs                   # LLM client, streaming (v3)

  # --- TOOLS (carry from v3) ---
  tools/
    mod.rs
    loop.rs                  # agentic loop driver
    builtin/                 # read, write, edit, grep, shell, etc.

  # --- INFRASTRUCTURE (carry from v3, slim down) ---
  daemon/
    mod.rs                   # startup/shutdown
    server.rs                # event loop (delegates to engine)
    supervisor.rs            # reads restart policy from YAML

  ipc/                       # carry from v3
  tui/                       # carry from v3
  worktree/                  # carry from v3
  scorer/                    # carry from v3
  config/
    mod.rs                   # YAML loading, schema validation
    schema.rs                # config struct definitions

# --- YAML DEFINITIONS (the "Ottofile" of orchestration) ---
strategies/
  fsm/
    work.yml
    bundle.yml
    plan.yml, spec.yml, phase.yml
    coordinator.yml
  roles/
    coordinator.yml          # capabilities, model, pool, iterations
    implementer.yml
    reviewer.yml
    researcher.yml
    integrator.yml
  recovery/
    default.yml              # v3 behavior expressed as YAML
    ask-a-friend.yml         # new composition example
  decomposition/
    full.yml                 # Plan->Spec->Phase->Work
    brief.yml                # Plan->Work
  scoring/
    default.yml              # 40/30/20/10 weights
  policies/
    work-sla.yml             # timeouts, attempt limits
    supervision.yml          # restart backoff
    queue-priority.yml       # priority formula
```

## Design Docs (in order)

1. **v4 architecture overview** — the runtime/strategy split, why, migration approach
2. **Primitive vocabulary** — every atomic operation, its inputs/outputs/preconditions
3. **FSM-in-YAML** — schema, runtime interpreter, startup validation
4. **Strategy composition** — triggers, actions, guards, wiring, lifecycle
5. **Decomposer as strategy** — how decomposition becomes pluggable
6. **AR trial integration** — how trial configs compose strategies, the scoring loop

Each builds on the previous. Doc 1 is the "why and what." Docs 2-4 are the engine.
Doc 5 proves the engine can express real behavior. Doc 6 closes the loop to AR.

## Key Design Principles

- **v3 behavior is the first strategy**: the YAML schema must be able to express everything
  v3 does today. If it can't, the schema is incomplete.
- **Sane defaults = v3 defaults**: zero-config behavior is identical to v3.
- **Fail fast on bad YAML**: validate all strategy definitions at startup, cross-reference
  against FSM definitions, reject invalid compositions before any work starts.
- **Compile-time safety trades for runtime flexibility**: we lose some compiler guarantees
  (e.g., exhaustive FSM state matching) in exchange for YAML-composable behavior.
  Startup validation must close that gap.
- **Primitives are the stability boundary**: primitives are well-tested Rust functions that
  rarely change. Strategies are YAML that changes per experiment. The boundary between
  them is the API contract.
