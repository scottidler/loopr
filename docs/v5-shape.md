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
| `derive` | Procedural macros (`Fsm`, `Record`); derives only, no fn-like or attribute macros | - |
| `domain` | Domain records, FSM const transition tables, TaskStore wrapper | `derive` |
| `runtime` | LLM client, tool trait, context builder, worktree lifecycle | `domain` |
| `decomposer` | Goal to Plan to Spec to Phase to Work DAG | `domain`, `runtime` |
| `agents` | Ralph loops for Implementer, Reviewer, Researcher, Director | `domain`, `runtime` |
| `integrator` | Accepted Bundles to Tick (deterministic, non-LLM) | `domain`, `runtime` |
| `ipc` | Typed daemon-client wire protocol (messages + framing, no transport) | `domain` |
| `loopr` | Binary: daemon loop + CLI dispatch + IPC transport + (later) TUI launcher | all of the above |

Directory structure:

```
loopr-v5/
├── Cargo.toml                    workspace manifest
├── docs/                         shape docs, design docs (motivated by runs)
├── crates/
│   ├── derive/
│   ├── domain/
│   ├── runtime/
│   ├── decomposer/
│   ├── agents/
│   ├── integrator/
│   ├── ipc/
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

### ipc

The daemon-client wire protocol. Typed messages, serde framing, no transport.

- `Request`, `Response`, `Event` as tagged enums with `deny_unknown_fields`.
- Framing choice (length-prefixed vs. newline-delimited) lives here, decided once, documented.
- Round-trip tests: message to bytes to message, byte stability, forward/backward compat.
- No `tokio`, no sockets. Transport is the consumer's job.

### loopr

The driver.

- `main.rs` parses CLI, forks-to-daemon or connects-as-client (pattern from `docs/v2-proven-patterns.md`).
- Daemon holds TaskStore, runs the reactive loop, threads records through the stage crates.
- IPC transport (async socket acceptance, connection lifecycle) lives here; the protocol itself is in `ipc`.
- CLI subcommands mirror stage boundaries: `loopr plan`, `loopr decompose`, `loopr execute`, `loopr integrate`, `loopr experiment`.
- TUI is a later, separate crate (see "Deferred Enhancements" and "Explicitly Not in First Gate").

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

## Observability

v5 overrides the `rules/rust.md` default of `log` + `env_logger` and uses **`tracing` + `tracing-subscriber` + `tracing-appender`**. Reason: a multi-crate daemon with long-lived async stages needs span-level context that survives across tasks and crate boundaries. The `log` crate gives flat events; `tracing` gives the span hierarchy that makes "follow one Work from Plan through Tick" tractable. Observability is a first-class concern in v5, owned by its own crate (`telemetry`).

### Three-layer log strategy

1. **Universal structured log** at `.loopr/runs/<run-id>/events.log`, JSON format. Every event carries `crate`, `span` hierarchy, `run_id` / `plan_id` / `work_id` when in scope, `level`, `ts`, and arbitrary kv. One file per run, grep- and jq-friendly, full history.
2. **Pretty per-run log** at `.loopr/runs/<run-id>/loopr.log`, same events formatted for humans. Mirrored to console at INFO+ during interactive runs.
3. **Per-Work fanout files** at `.loopr/runs/<run-id>/work/<work-id>.log`. A subscriber watches the `work_id` span and splits a file per Work. Deferred until Stage 7; infrastructure is present in Stage 2.

### Run identifier

`run_id = YYYYMMDD-HHMMSS` in local time; e.g., `20260418-123653`. If two runs start in the same second, the daemon appends `-N` where N increments from 2: `20260418-123653-2`. The first run gets the clean name; disambiguator is rare and only when necessary.

### Span conventions

- `stage.<name>`: a pipeline stage (`stage.decompose`, `stage.implement`, `stage.review`, `stage.integrate`).
- `ralph.<role>`: one iteration of a ralph loop (`ralph.implementer`, `ralph.reviewer`).
- `tool.<name>`: one tool invocation (`tool.bash`, `tool.edit`).
- Every span carries `run_id`; spans within a Plan carry `plan_id`; spans within a Work carry `work_id`.

## Target Repo Layout

Loopr operates on **other** repos, not on itself. When pointed at a target, it manages two sibling directories at the target's root and leaves the rest of the target untouched.

```
<target>/
├── .taskstore/              committed (JSONL is truth, merge driver resolves conflicts)
│   ├── plans.jsonl
│   ├── specs.jsonl
│   ├── works.jsonl
│   ├── bundles.jsonl
│   ├── ticks.jsonl
│   ├── .version             schema version
│   └── .gitignore           taskstore-managed: excludes its .db cache
├── .loopr/                  NOT committed (listed in .git/info/exclude)
│   ├── runs/
│   │   └── 20260418-123653/
│   │       ├── events.log   structured JSON, per-run
│   │       └── loopr.log    pretty format, per-run
│   ├── config.yml           per-repo loopr overrides (local only)
│   ├── socket               Unix domain socket for daemon<->client IPC
│   ├── daemon.pid           PID lockfile; one daemon per target
│   └── worktree-registry.jsonl   list of active agent worktrees
├── .gitattributes           committed; sets taskstore merge driver on *.jsonl
└── (sibling worktrees outside the target, not inside .loopr/)
```

### Rationale for the dual-directory split

- **`.taskstore/`** holds the truth (Plan/Spec/Phase/Work/Bundle/Tick records as JSONL). Committed so collaborators share state; taskstore's git merge driver resolves concurrent edits by timestamp.
- **`.loopr/`** holds transient state (logs, socket, pid, per-repo config). Ephemeral, machine-local, never committed. Carries `.git/info/exclude` rather than `.gitignore` so loopr does not pollute the target's committed `.gitignore`.
- **Sibling worktrees** (v3/v4 pattern) live outside the target repo entirely, typically at `<target-parent>/<target-name>-work-<work-id>/`. Git's ignore rules inside the worktree don't accidentally exclude files the agent writes. Only a registry of active worktrees lives inside `.loopr/`.

### CLI targeting: `-C <path>` (git-style)

- `loopr` uses **CWD** as the effective target by default.
- `loopr -C <path> <subcommand>` changes the effective target before any other logic runs, matching `git -C`.
- `LOOPR_TARGET` environment variable is a fallback when neither CLI nor CWD is definitive.

### Source-guard

`loopr`, before operating on an effective target, walks up from the target directory to `/` looking for the sentinel file `.loopr-source-guard`. If found, it refuses with a clear error. The sentinel is committed at the root of this loopr-v5 repo. Protects against v3/v4's recurring confusion where an agent treated the loopr source tree as the target.

### `loopr init` on a new target

Single idempotent command run inside the target (or via `-C`):

1. Create `.loopr/` and seed it with an empty `config.yml`.
2. Open the TaskStore via `Store::open(".")`, which creates `.taskstore/` on first call.
3. Install taskstore's git hooks and merge driver (`taskstore install-hooks`).
4. Append `.loopr/` to `.git/info/exclude` (not `.gitignore`) so the local loopr state is ignored without polluting the target's committed ignore list.
5. Verify the target is not a loopr source tree (source-guard check).

## Process Rules

The rules that change the failure mode of v1–v4, not the rules that the architecture already enforces:

1. **One design doc at a time, motivated by a failing run.** No new detailed doc until the previous one has produced a passing E2E against a real target repo. `docs/v5-shape.md` is the exception — it's the seed.
2. **Seam tests, not only unit tests.** Every crate boundary has at least one golden-file test: given this input serialized, produce this output. Serde round-trip tests cover `deny_unknown_fields`. Unit tests on one side of a seam are not enough.
3. **No coexistence migrations.** If a stage needs to change, change it in place or replace it in one commit. No dual paths. No "both systems run during migration."
4. **One architectural pivot per quarter, at most.** And only after three consecutive passing E2Es against real projects. v3/v4's four paradigm shifts in 55 days is the exact tempo not to repeat.

## First Gate

v5 is "real" when the following can be run end-to-end against a real git repo:

```
loopr -C ~/repos/scottidler/rust-version daemon start
loopr -C ~/repos/scottidler/rust-version plan \
  "Add a --version flag that prints CARGO_PKG_VERSION to stdout"
```

…and the daemon:

1. Decomposes the goal into one or more Work items with deps and AC.
2. Spawns an Implementer in a sibling worktree, produces a Bundle.
3. Spawns a Reviewer, gets a Verdict.
4. If approved, Integrator merges into the integration branch, runs validation, publishes a Tick.
5. Tick merges to main.

No YAML composition engine involved. No Director unless escalation triggers. No Researcher unless tool discovery needed. The trivial path works before anything else is added.

This is the same scenario that succeeded once on v4 (`rust-version`, v0.1.121). v5's gate: reproduce that, on a fresh `rust-version` target (created with the `scaffold-rust-repo` skill when Stage 9 begins), through the typed multi-crate pipeline.

## Explicitly Not in First Gate

To avoid scope creep into v4.2:

- No Plan/Spec/Phase/Work hierarchy with >1 level (start with flat Work list until flat proves insufficient).
- No Director (escalation turns into exit-with-error until escalation is motivated by a real stuck run).
- No AutoResearch harness (wire configs, but no sweep/score loop until the baseline runs).
- No parallel worktrees (one Work at a time until serial proves the shape).
- No semantic bubble-up / coverage evaluator.

These are earned features, added when a real run fails for lack of them.

## Deferred Enhancements

Ideas evaluated and not first-gate scope, kept here so future sessions don't re-derive them from scratch. Each line is a pointer, not a design - the detailed design doc gets written when the enhancement is motivated by a real run.

1. **Typed event bus inside the daemon.** Pattern #6 from the leaked Claude Code architecture: structured streaming events (`WorkStatusChanged { work_id, from, to }`-style) that subscriber agents react to instead of polling TaskStore. Earn it when polling becomes the bottleneck, or when the TUI needs to watch the same stream agents do.

2. **Supersession over deletion for record revisions.** Cloudflare Agent Memory pattern: when a Plan gets re-decomposed or a Bundle gets superseded by a fix, keep the old record with a forward pointer instead of dropping it. Good audit trail, matches the Ralph-loop retry ethos. Earn it when decomposition re-runs start losing history we want back.

3. **Graph memory for fast record recall.** Cersei's Grafeo hits indexed lookups in ~98μs vs. Claude Code's 7.5s LLM-based relevance rank. Worth understanding the mechanism before committing to TaskStore's query story. Earn it when the TUI or agents need queries like "which Work modified this file in the last month" without burning an LLM call.

4. **Cersei as a reference to read, not a dependency to adopt.** [pacifio/cersei](https://github.com/pacifio/cersei) is a Rust SDK for coding agents (tool execution, LLM streaming, sub-agent orchestration, graph memory, MCP). Two specific patterns worth studying before we implement corresponding v5 layers: their tool derive macro ergonomics (directly informs our `derive` crate's future `#[derive(Tool)]`) and Grafeo's graph memory mechanism (item 3). Do NOT adopt as a `runtime` dependency - v5's whole point is owning the typed seams, which means owning the code that sits at them; adopting Cersei at our most central crate would repeat v4's failure pattern of fighting a foreign abstraction.

5. **LLM response cache at `~/.local/share/loopr/llm-cache/`.** Keyed by prompt hash. Cross-repo dedup (same prompt on multiple targets hits the cache once). Optional, disable-able. XDG path, not per-target. Earn it when repeated LLM calls on similar prompts across targets become a cost problem.

6. **Global runs-index at `~/.local/share/loopr/runs-index.jsonl`.** Tiny append-only index: `{run_id, target_path, started_at, goal, outcome}` per run. Enables `loopr runs list --all` cross-target queries ("which repo was that demo I did last week"). Cheap, powerful, XDG path. Earn it when "I can't remember which repo" becomes a question worth answering in code.

## Open Questions

Kept sparse on purpose. Only questions that block first-gate work. Decisions made during the scaffolding phase are recorded as closed; leave them listed so future sessions see the context.

**Closed:**
- **TaskStore:** git dep on `scottidler/taskstore`. (No vendoring.)
- **Fsm derive:** port `#[derive(Fsm)]` from v4's `loopr-derive`. Rehomed in `crates/derive` under v5's single-word naming convention. New `#[derive(Record)]` joins it; derives only, no function-like or attribute macros.
- **Tool registry:** minimal for first gate ({Read, Write, Edit, Bash, Grep, Glob}). Expands toward Claude / Gemini / Codex parity as TUI Chat and agent capability needs earn each addition.
- **Observability:** `tracing` + `tracing-subscriber` + `tracing-appender`, owned by `crates/telemetry`. Overrides the `rules/rust.md` default of `log` + `env_logger` for v5 specifically.
- **Target-repo state:** `.taskstore/` (committed truth) + `.loopr/` (transient, excluded via `.git/info/exclude`).

**Open:**
- None blocking first-gate work at the moment. New questions get added here when they surface.
