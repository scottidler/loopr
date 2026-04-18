# Loopr v5 Shape

**Author:** Scott A. Idler
**Date:** 2026-04-18
**Status:** Seed (not a design doc, not a plan — the architectural shape we're building toward)

## Summary

v5 is a clean-break rewrite of Loopr on an orphan branch. The shape is a compiler-style pipeline of typed stages, each living in its own crate. Seams between stages are Rust function calls with typed arguments, not string-keyed dictionaries. AutoResearch-style experimentation is preserved as config-driven parameter sweeping, not as a YAML-composable orchestration engine.

## What v5 Is

A workspace of 6 crates. Each crate owns one stage of the pipeline and exports a small, typed public API. The binary crate (`loopr`) is the driver that threads state through the stages.

```
Goal ──► decomposer ──► Work DAG ──► agents ──► Bundles ──► integrator ──► Tick
                                                                                                    │
                                                                                                    ▼
                                                                                              main branch
```

All persistence flows through `domain` (TaskStore + FSM). All LLM calls, tool execution, and worktree lifecycle flow through `runtime`. Both are depended on by the stage crates and the binary.

## What v5 Is Not

Explicit non-goals, extracted from v1–v4 post-mortems:

- **Not a YAML-composable orchestration engine.** v4's composition engine turned every stage boundary into a `HashMap<String, Value>` and produced a class of seam-drift bugs (kebab/snake mismatch, uncalled `validate_params`, ignored idempotency declarations) that cannot exist when seams are typed Rust functions.
- **Not a durable workflow engine.** Loopr is reactive: the daemon holds state, receives events, runs relevant stages incrementally. No Temporal-style suspend/resume, no saga rollback. Self-healing comes from the daemon re-evaluating state each tick, same as v3/v4.
- **Not a coexistence migration.** v1 to v2 was a rewrite. v2 to v3 was a rewrite. v3 to v4 was a coexistence migration and it failed. v5 does not dual-path with v4. If v5 earns its place, v4 gets archived.
- **Not a pre-designed spec for every stage.** One shape doc (this one). Detailed design docs are motivated by failing runs, not written in advance. Self-reviewed "5/5 review passes" on unverified specs is the failure mode to avoid.
- **Not a rebuild of everything from first principles.** Worktree management, TUI rendering, IPC framing, tool registry, LLM streaming, context budgeting — these were v3 wins and they carry over inside `runtime` without reargument.

## Crate Layout

| Crate | Responsibility | Depends On |
|---|---|---|
| `domain` | Domain records, FSM const transition tables, TaskStore wrapper | — |
| `runtime` | LLM client, tool trait, context builder, worktree lifecycle | `domain` |
| `decomposer` | Goal to Plan to Spec to Phase to Work DAG | `domain`, `runtime` |
| `agents` | Ralph loops for Implementer, Reviewer, Researcher, Director | `domain`, `runtime` |
| `integrator` | Accepted Bundles to Tick (deterministic, non-LLM) | `domain`, `runtime` |
| `loopr` | Binary: daemon, IPC, TUI, CLI dispatch | all of the above |

Directory structure:

```
loopr-v5/
├── Cargo.toml                    workspace manifest
├── docs/                         shape docs, design docs (motivated by runs)
├── crates/
│   ├── domain/
│   ├── runtime/
│   ├── decomposer/
│   ├── agents/
│   ├── integrator/
│   └── loopr/
```

## ABI Contracts

Each crate exports a typed public surface. Crossing crate boundaries is a Rust function call, not a JSON dispatch. `deny_unknown_fields` on every serde type is the norm.

### domain

The symbol layer. Pure data and invariants.

- `Plan`, `Spec`, `Phase`, `Work`, `Bundle`, `Tick` — record types with `Record` impls for persistence.
- `FsmTransition<T>` — const transition tables with role guards. A transition that isn't in the table is a compile-time mismatch, not a runtime YAML lookup.
- `Store` — the TaskStore wrapper; JSONL is truth, SQLite is cache (invariant carried over from v3/v4).

### runtime

The services layer. Effectful but generic over roles and stages.

- `LlmClient` trait — swappable LLM backends. Default impl is Anthropic Messages API with SSE streaming.
- `Tool` trait with typed `Input`/`Output` — one builtin per file under `tools/`. Tool schemas validated via serde at registration, not at every call.
- `ContextBuilder` — token-budgeted prompt assembly.
- `Worktree` — git worktree handle with guaranteed cleanup.

### decomposer

The middle-end. Plan production and decomposition.

- `fn plan(goal: &Goal, ctx: &mut Context) -> Result<Plan>` — user intent to validated Plan.
- `fn decompose(plan: &Plan, ctx: &mut Context) -> Result<WorkDag>` — Plan to Spec/Phase/Work DAG with typed deps.
- Strategies (brief, full, custom) are `impl DecomposeStrategy` selected by config, not composed from YAML primitives.

### agents

The backend execute stages. Ralph loops per role.

- `fn run_implementer(work: &Work, ctx: &Context) -> Result<Bundle>`
- `fn run_reviewer(bundle: &Bundle, ctx: &Context) -> Result<Verdict>`
- `fn run_researcher(query: &Query, ctx: &Context) -> Result<Finding>`
- `fn run_director(event: &Event, ctx: &mut Context) -> Result<Action>`

Retry / escalation / advisor strategies are `impl RetryStrategy` / `impl EscalationStrategy` selected by config, not composed from YAML triggers.

### integrator

The linker. Deterministic, non-LLM.

- `fn integrate(bundles: &[Bundle]) -> Result<Tick, IntegrationError>`
- Same bundles plus same base equals same Tick SHA or same typed conflict error. No LLM imports in this crate.

### loopr

The driver.

- `main.rs` parses CLI, forks-to-daemon or connects-as-client (pattern from `docs/v2-proven-patterns.md`).
- Daemon holds TaskStore, runs the reactive loop, threads records through the stage crates.
- TUI is the visual debugger: every stage's input and output is inspectable.
- CLI subcommands mirror stage boundaries: `loopr plan`, `loopr decompose`, `loopr execute`, `loopr integrate`, `loopr experiment`.

## AutoResearch Support

The v4 motivation was correct; the mechanism was wrong. v5 separates parameters (config, sweepable) from orchestration logic (typed Rust, not sweepable).

Every crate defines its own config struct in `config.rs`, composed at the top level:

```
domain/src/config.rs        Config (top-level, composed of sub-configs)
decomposer/src/config.rs  DecomposerConfig
agents/src/config.rs      AgentsConfig { implementer, reviewer, researcher, director }
integrator/src/config.rs  IntegratorConfig
runtime/src/config.rs     RuntimeConfig
```

Loaded from one YAML file at startup. Serde validates, with `deny_unknown_fields` catching typos as startup errors with line numbers.

Strategy variants are pluggable Rust traits selected by config name:

```rust
pub trait RetryStrategy: Send + Sync { ... }
pub struct MaxAttemptsRetry { max: u32 }
pub struct AdvisorAssistedRetry { advisor_model: ModelId, max_depth: u32 }
```

```yaml
# config variant for AR sweep
agents:
  implementer:
    retry-strategy:
      kind: advisor-assisted
      advisor-model: claude-opus-4-6
      max-depth: 2
```

AR generates variant YAMLs and runs them:

```
loopr experiment --config variants/v47.yml --target rust-version
  → runs to completion
  → writes score to experiments/<run-id>.json
  → harness ranks configs
```

Novel strategies mean writing a new `impl RetryStrategy` in Rust. Novel topologies mean human-designed then exposed for AR to tune. AR is for parameter sweeping, not structural invention — and that's what AR is actually good at.

## Process Rules

The rules that change the failure mode of v1–v4, not the rules that the architecture already enforces:

1. **One design doc at a time, motivated by a failing run.** No new detailed doc until the previous one has produced a passing E2E against a real target repo. `docs/v5-shape.md` is the exception — it's the seed.
2. **Seam tests, not only unit tests.** Every crate boundary has at least one golden-file test: given this input serialized, produce this output. Serde round-trip tests cover `deny_unknown_fields`. Unit tests on one side of a seam are not enough.
3. **No coexistence migrations.** If a stage needs to change, change it in place or replace it in one commit. No dual paths. No "both systems run during migration."
4. **One architectural pivot per quarter, at most.** And only after three consecutive passing E2Es against real projects. v3/v4's four paradigm shifts in 55 days is the exact tempo not to repeat.

## First Gate

v5 is "real" when the following can be run end-to-end against a real git repo:

```
loopr daemon start
loopr plan "Add a --version flag that prints CARGO_PKG_VERSION to stdout"
```

…and the daemon:

1. Decomposes the goal into one or more Work items with deps and AC.
2. Spawns an Implementer in a worktree, produces a Bundle.
3. Spawns a Reviewer, gets a Verdict.
4. If approved, Integrator merges into the integration branch, runs validation, publishes a Tick.
5. Tick merges to main.

No YAML composition engine involved. No Director unless escalation triggers. No Researcher unless tool discovery needed. The trivial path works before anything else is added.

This is the same scenario that succeeded once on v4 (`rust-version`, v0.1.121). v5's gate is: reproduce that, against the same target, with the 6-crate pipeline and typed seams.

## Explicitly Not in First Gate

To avoid scope creep into v4.2:

- No Plan/Spec/Phase/Work hierarchy with >1 level (start with flat Work list until flat proves insufficient).
- No Director (escalation turns into exit-with-error until escalation is motivated by a real stuck run).
- No AutoResearch harness (wire configs, but no sweep/score loop until the baseline runs).
- No parallel worktrees (one Work at a time until serial proves the shape).
- No semantic bubble-up / coverage evaluator.

These are earned features, added when a real run fails for lack of them.

## Open Questions

Kept sparse on purpose. Only questions that block first-gate work:

- Do we carry over TaskStore from scottidler/taskstore as a git dep, or vendor a minimal in-crate implementation until the Record trait surface stabilizes?
- Do we keep `#[derive(Fsm)]` from `loopr-derive` (the proc macro), or write the const transition tables by hand in v5?
- Does `runtime` carry over the v4 tool registry wholesale, or is it scoped down to the smallest builtin set needed for first gate?
