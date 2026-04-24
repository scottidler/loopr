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
| `telemetry` | Tracing subscriber init, run-id allocation, span conventions, log-query helpers | - |
| `store` | Typed wrapper around `scottidler/taskstore`; JSONL + SQLite cache + git-hooks install; anti-corruption layer | `derive`, `taskstore` |
| `domain` | Records + FSM const transition tables only; no I/O, no persistence | `derive`, `taskstore-traits` (trait-only; no I/O transitively) |
| `llm` | `LlmClient` trait + Anthropic backend with SSE streaming; model tier resolution. **No prompt assembly** — that's `agents`. | `domain`, `telemetry` |
| `tools` | `Tool` trait + built-in tool impls (`Read`, `Write`, `Edit`, `Bash`, `Grep`, `Glob`) + lane classification (`Local`/`Net`/`Heavy`) + bwrap sandbox integration | `domain`, `telemetry` |
| `worktree` | Sibling git worktree lifecycle + registry + daemon-startup crash recovery | `domain`, `telemetry` |
| `ipc` | Typed daemon-client wire protocol (messages + framing, no transport) | `domain` |
| `context` | Prompt assembly (handlebars templates + partials + token budgeting); shared by decomposer and agents | `domain`, `store`, `tools`, `telemetry` |
| `decomposer` | Goal to Plan to Spec to Phase to Work DAG | `domain`, `store`, `llm`, `context` |
| `agents` | Ralph loops per role | `domain`, `store`, `llm`, `tools`, `worktree`, `context` |
| `integrator` | Accepted Bundles to Tick (deterministic, non-LLM); the Cargo graph mechanically forbids an `llm` dep | `domain`, `store`, `worktree` |
| `loopr` | Binary: daemon loop + CLI dispatch + IPC transport + (later) TUI launcher | all of the above |

**Note on `taskstore` in `domain`:** `domain` uses `taskstore_traits::Record` — the trait-only crate extracted from `scottidler/taskstore` (workspace release `v0.3.0`, traits crate at `taskstore-traits v0.1.0`). Trait content is pure `serde` + `std`, so `domain`'s transitive compile graph stays clean (no `rusqlite` / `fs2` / `chrono`). Persistence-heavy deps are confined to `store`, which depends on the full `taskstore` crate. The two git deps are declared in the root `[workspace.dependencies]` block and inherited via `workspace = true`; this structurally prevents the split-brain failure mode where two members pick different commits and Rust instantiates `taskstore-traits` twice with incompatible `Record` type identities. Integration details in [taskstore-integration.md](taskstore-integration.md).

Directory structure:

```
loopr-v5/
├── Cargo.toml                    workspace manifest
├── docs/                         shape docs, design docs (motivated by runs)
├── crates/
│   ├── derive/
│   ├── telemetry/
│   ├── store/
│   ├── domain/
│   ├── llm/
│   ├── tools/
│   ├── worktree/
│   ├── ipc/
│   ├── context/
│   ├── decomposer/
│   ├── agents/
│   ├── integrator/
│   └── loopr/
```

## ABI Contracts

Each crate exports a typed public surface. Crossing crate boundaries is a Rust function call, not a JSON dispatch. `deny_unknown_fields` on every serde type is the norm.

### telemetry

Observability foundation. First-class, its own crate.

- `fn init(target_dir: &Path, run_id: RunId) -> Result<Guard>` — composes the `tracing-subscriber` layers (JSON file, pretty file, console mirror at INFO+) and returns a drop-guard that flushes on shutdown.
- `RunId` — newtype wrapping a `YYYYMMDD-HHMMSS[-N]` string; allocated atomically by the daemon.
- Span naming conventions: `stage.<name>`, `ralph.<role>`, `tool.<name>`. `run_id` / `plan_id` / `work_id` carried as span fields.
- Log-query back-end for `loopr logs` CLI subcommands.
- No `tokio`, no LLM, no network deps.

### store

The persistence anti-corruption layer. Wraps `scottidler/taskstore` with type-safe accessors.

- `Store::open(target_path) -> Result<Store>` — opens `.loopr/taskstore/` at the target (see `store::TASKSTORE_SUBPATH`), initializes schema, installs git hooks, syncs if stale.
- Typed collection accessors: `store.plans()`, `store.specs()`, `store.phases()`, `store.works()`, `store.bundles()`, `store.ticks()`. Each returns a handle that enforces the record type on read/write.
- `StoreError` — typed errors; `rusqlite` / `fs2` / `taskstore`-internal types do NOT leak out.
- No LLM, no subprocess execution, no network.

### domain

The pure symbol layer. Records and invariants only.

- `Plan`, `Spec`, `Phase`, `Work`, `Bundle`, `Tick` — record structs with `#[derive(Fsm, Record)]`.
- `#[derive(Fsm)]`-generated state machines — compile-time transition dispatch with role guards expanded as `match (from, to)` arms. A transition that isn't in the table is a compile-time mismatch, not a runtime YAML lookup. Transition tables live inside each status enum's generated `validate_transition` body; there is no publicly-exposed `FsmTransition<T>` wrapper type. Rich error context (valid targets per state, with and without overrides) is pre-generated at macro-expansion time into const slices stored on the `FsmError<S>` returned from a rejection. See [`docs/design/2026-04-20-fsm-macro.md`](design/2026-04-20-fsm-macro.md).
- `Status`, `Role`, `Tier`, and typed IDs (`PlanId`, `WorkId`, `RunId`, ...).
- No I/O. No `rusqlite`, no `fs2`, no `tokio`. Compile-time enforced by omission of those deps from `domain/Cargo.toml`.

### llm

The LLM API-bounds crate. Agnostic of what we say; owns how we say it on the wire.

- `LlmClient` trait — swappable LLM backends. Default impl is Anthropic Messages API with SSE streaming, retry on transient errors.
- Model tier resolution: given a tier name (`primary`, `lightweight`, `advisor`) or a literal model ID, returns the concrete model to call. Records the concrete returned model ID on every span.
- Cost accounting: emits `tracing` spans with `input_tokens`, `output_tokens`, `cost_usd`, and the concrete model ID.
- **Does NOT do prompt assembly.** `llm` takes ready-to-send `Message` vectors. `agents::ContextBuilder` is the only place that assembles them (because it's the only crate that sees `domain` + `store` + `llm` + `tools` simultaneously).

### tools

The subprocess and capability layer. Tools are agent-callable capabilities with typed input/output and lane-based isolation.

- `Tool` trait with typed `Input` / `Output` / `Error`.
- Built-in tool impls (first-gate set: `Read`, `Write`, `Edit`, `Bash`, `Grep`, `Glob`); one per file under `src/tools/`.
- Lane classification (`fn classify(tool_name) -> Lane`): `Local` (no-network, `bwrap --unshare-net`, 10 slots, 30s/60s), `Net` (network allowed, no sandbox, 5 slots, 60s/120s), `Heavy` (network allowed, 1-slot serial, 600s/1800s — for builds/tests/lints).
- `LaneRouter`: enforces per-lane concurrency via tokio semaphores.
- bwrap sandbox integration + the `security.sandbox: required | preferred | off` posture logic.
- Bash denylist (base + target extensions).
- Tool schemas are exposed for `agents::ContextBuilder` to render into prompts.

### worktree

Flat-interior per-attempt git worktree lifecycle. Infrastructure-only: exposes primitives, does not orchestrate Work-state transitions.

- `Worktree::create(repo_path, worktree_root, work_id, base_sha) -> Result<Worktree>` — provisions `<target>/.loopr/worktrees/<work-id>-<seq>/` on branch `loopr/wk-<work-id>-<seq>`. `seq` is allocated internally via EEXIST-retry using git's own "already exists" stderr phrases (with `LC_ALL=C` forced for locale stability).
- `Worktree::cleanup(self)` — explicit cleanup: removes the worktree, **keeps the branch** (integrator still needs it to merge). `Drop` is a crash-safety net only; routine cleanup uses explicit `.cleanup()` inside `tokio::task::spawn_blocking` so the synchronous `git worktree remove` doesn't starve the tokio executor.
- `worktree::delete_branch(repo_path, branch)` — integrator calls after a Tick publishes; idempotent on missing.
- `worktree::list(repo_path, worktree_root)`, `parse_branch(branch) -> Option<(WorkId, u32)>`, `cleanup_at(repo_path, path)`, `ensure_loopr_excludes(repo_path)` — primitives consumed by the `loopr`-binary reconcile routine.
- `AttemptCleanupPolicy` enum — `Immediate`/`OnWorkTerminal`/`OnRunEnd`/`Never`, default `OnWorkTerminal`. Exposed via `.loopr/config.yml` + ENV + CLI.
- Internal git invocations via `std::process::Command` directly; `worktree` does NOT go through the `tools` crate (infrastructure, not an LLM-facing tool). **No registry file** — git's own worktree registry (`git worktree list --porcelain`) + the `loopr/wk-*` branch prefix + TaskStore is a complete, race-free reconcile path.
- **Reconcile is NOT in this crate.** Crash recovery lives in the `loopr` binary (`loopr::daemon::startup::reconcile`) because it requires joining git state with TaskStore + `Work`-FSM mutations; `worktree` stays infrastructure-only. See "Worktree crash recovery" below.

Amended 2026-04-21 (a1/a2/a3/a4/a5); see Amendments section and `docs/design/2026-04-21-worktree-lifecycle.md` for the full rationale.

### decomposer

The middle-end. Plan production and decomposition.

- `fn plan(goal: &Goal, ctx: &mut Context) -> Result<Plan>` — user intent to validated Plan.
- `fn decompose(plan: &Plan, ctx: &mut Context) -> Result<WorkDag>` — Plan to Spec/Phase/Work DAG with typed deps.
- Strategies (brief, full, custom) are `impl DecomposeStrategy` selected by config, not composed from YAML primitives.

### context

Prompt assembly. Shared by `decomposer` and `agents` (the two LLM-calling pipeline crates). Produces ready-to-send `Vec<Message>` from typed inputs; does not talk to the LLM itself.

- `ContextBuilder` — token-budgeted prompt assembly. Renders `domain` records, persisted history from `store`, and `tools` schemas into a `Vec<Message>`. Uses handlebars-rust with partials for SSOT; templates loaded via the three-layer chain `.loopr/prompts/` → `~/.config/loopr/prompts/` → baked-in (via `include_dir!()`).
- Typed per-role entry points: `build_for_plan(goal)`, `build_for_decompose(plan)`, `build_for_implementer(work, tools_snapshot)`, `build_for_reviewer(bundle)`, etc.
- Emits `tracing` spans carrying rendered-token counts, template paths resolved, partials invoked.
- Does NOT depend on `llm` — produces Messages; does not send them.

### agents

The backend execute stages. Ralph loops per role.

- `fn run_implementer(work: &Work, deps: &Deps<...>) -> Result<Bundle>`
- `fn run_reviewer(bundle: &Bundle, deps: &Deps<...>) -> Result<Verdict>`
- `fn run_researcher(query: &Query, deps: &Deps<...>) -> Result<Finding>`
- `fn run_director(event: &Event, deps: &Deps<...>) -> Result<Action>`
- Prompt assembly happens in `context` (shared with `decomposer`); agents consume the assembled Messages and orchestrate the LLM/tool/worktree/store calls.
- Dependency injection via the `Deps<L, T, W, S, C>` struct pattern (one generic param flows through signatures; concrete trait bounds live on the struct). Reserves the "no `dyn` dispatch" rule while avoiding contagious multi-generic boilerplate (Architect Round 3 ergonomic warning).

Retry / escalation / advisor strategies are `impl RetryStrategy` / `impl EscalationStrategy` selected by config, not composed from YAML triggers.

`agents` is the widest-scope crate in v5 (depends on six others including `context`). The testability strategy — `Deps<...>` + per-trait fakes — keeps the Architect's Round 2 junk-drawer concern bounded. If `src/` pushes 1500 lines, split per-role as module directories first; per-role sub-crates only if the module split proves insufficient.

### integrator

The linker. Deterministic, non-LLM.

- `fn integrate(bundles: &[Bundle]) -> Result<Tick, IntegrationError>`
- Same bundles plus same base equals same Tick SHA or same typed conflict error.
- **Mechanically cannot import `llm`.** `integrator/Cargo.toml` has no `llm` dep, enforced at the Cargo graph level. The Round 1 Architect flagged the previous `runtime` monolith as a contradiction because `integrator` transitively pulled in `LlmClient`; this split makes the no-LLM rule enforceable by the compiler, not just by review.

### ipc

The daemon-client wire protocol. Typed messages, serde framing, no transport.

- `DaemonRequest { id: u64, method: String, params: Value }`, `DaemonResponse { id: u64, result: Option<Value>, error: Option<RpcError> }`, `DaemonEvent { event: String, data: Value }` — JSON-RPC-style envelope, inherited verbatim from v3/v4.
- `IpcMessage` enum discriminates between `Response` and `Event` on the client side.
- `RpcError { code: i32, message: String }` with named constants: `CODE_PARSE_ERROR = -32700`, `CODE_INVALID_REQUEST = -32600`, `CODE_METHOD_NOT_FOUND = -32601`, `CODE_INVALID_PARAMS = -32602`, `CODE_INTERNAL = -32603` (JSON-RPC 2.0 standard) plus loopr-specific `-32000` through `-32006` (transition-rejected, not-found, stale-bundle, validation-required, pool-exhausted, precondition-failed, protocol-version-mismatch). **Amended 2026-04-19** to add `-32700`/`-32600`/`-32006` per the Stage 3 design doc's architect review; original vision omitted them.
- **Framing: newline-delimited JSON** (one JSON object per line) using `tokio_util::codec::LinesCodec` with a **1 MiB max line**. Chosen to match v3/v4; the 1 MiB cap exists because cargo/clippy/test validation output can be substantial.
- Round-trip tests: message to bytes to message, byte stability, forward/backward compat.
- No `tokio`, no sockets. Transport is the consumer's job. (Note: the framing *choice* — `LinesCodec` — references `tokio_util`; the actual async read/write lives in `loopr` where it consumes `UnixStream`.)

### loopr

The driver.

- `main.rs` parses CLI, forks-to-daemon or connects-as-client (pattern from `docs/v2-proven-patterns.md`).
- Daemon holds TaskStore, runs the reactive loop, threads records through the stage crates.
- IPC transport (async socket acceptance, connection lifecycle) lives here; the protocol itself is in `ipc`.
- CLI surface (post-amendment `a6`) is plumbing-only: `loopr init`, `loopr plan "<goal>"`, `loopr plans|works|bundles|ticks`, `loopr show <id>`, `loopr logs {tail|runs}`, `loopr daemon {start|stop|status}`. Bare `loopr` and explicit `loopr tui` launch the TUI when that crate lands. Globals: `-C <path>`, `-l` / `--log-level`, `-o` / `--output {json|yaml}`. See `docs/design/2026-04-23-cli-plumbing-shape.md`.
- TUI is a later, separate crate (see "Deferred Enhancements" and "Explicitly Not in First Gate").

## AutoResearch Support

The v4 motivation was correct; the mechanism was wrong. v5 separates parameters (config, sweepable) from orchestration logic (typed Rust, not sweepable).

Every crate defines its own config struct in `config.rs`, composed at the top level:

```
loopr/src/config.rs         Config (top-level, composed of sub-configs)
store/src/config.rs         StoreConfig
llm/src/config.rs           LlmConfig (incl. models block with tiers)
tools/src/config.rs         ToolsConfig (incl. sandbox knob, denylist)
worktree/src/config.rs      WorktreeConfig
decomposer/src/config.rs    DecomposerConfig
agents/src/config.rs        AgentsConfig { implementer, reviewer, researcher, director }
integrator/src/config.rs    IntegratorConfig
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
3. **Per-Work fanout files** at `.loopr/runs/<run-id>/work/<work-id>.log`. A `WorkFanoutLayer` subscriber watches the `work_id` span and splits a file per Work. Built in Stage 2 (design doc: `docs/design/2026-04-19-telemetry-stage-2.md`); stays inert until Stage 7 emits the first `work_id`-bearing span.

### Run identifier

`run_id = YYYYMMDD-HHMMSS` in local time; e.g., `20260418-123653`. If two runs start in the same second, the daemon appends `-N` where N increments from 2: `20260418-123653-2`. The first run gets the clean name; disambiguator is rare and only when necessary.

### Span conventions

- `stage.<name>`: a pipeline stage (`stage.decompose`, `stage.implement`, `stage.review`, `stage.integrate`).
- `ralph.<role>`: one iteration of a ralph loop (`ralph.implementer`, `ralph.reviewer`).
- `tool.<name>`: one tool invocation (`tool.bash`, `tool.edit`).
- Every span carries `run_id`; spans within a Plan carry `plan_id`; spans within a Work carry `work_id`.

## Target Repo Layout

Loopr operates on **other** repos, not on itself. When pointed at a target, it manages a single top-level directory `.loopr/` at the target's root, with taskstore nested inside it. Nothing else under the target is touched.

```
<target>/
├── .loopr/                       single loopr-owned directory at the target root
│   ├── taskstore/                committed (JSONL is truth, merge driver resolves conflicts)
│   │   ├── plans.jsonl
│   │   ├── specs.jsonl
│   │   ├── works.jsonl
│   │   ├── bundles.jsonl
│   │   ├── ticks.jsonl
│   │   ├── .version              schema version
│   │   └── .gitignore            taskstore-managed: excludes its .db cache
│   ├── runs/                     NOT committed
│   │   └── 20260418-123653/
│   │       ├── events.log        structured JSON, per-run
│   │       └── loopr.log         pretty format, per-run
│   ├── config.yml                per-repo loopr overrides (local only, NOT committed)
│   ├── socket                    Unix domain socket for daemon<->client IPC (NOT committed)
│   ├── daemon.pid                PID lockfile, one daemon per target (NOT committed)
│   └── worktrees/                per-attempt agent worktrees (NOT committed; <work-id>-<seq>)
│       ├── wi-042-1/             first attempt for wi-042 (branch loopr/wk-wi-042-1)
│       └── wi-042-2/             second attempt (first was rejected)
└── .gitattributes                committed; sets taskstore merge driver on .loopr/taskstore/*.jsonl
```

Commit boundary runs **inside** `.loopr/`: `taskstore/` is committed (truth), everything else under `.loopr/` is transient and excluded via `.git/info/exclude`. The excludes pattern keeps `.loopr/taskstore/` in git while ignoring `.loopr/runs/`, `.loopr/socket`, `.loopr/daemon.pid`, etc.

### Rationale for nesting taskstore inside `.loopr/`

- **One top-level folder to reason about.** Every loopr-created file lives under a single `.loopr/` at the target root. Grep, ignore rules, permissions, and tab-completion all benefit.
- **`.loopr/taskstore/`** holds the truth (Plan/Spec/Phase/Work/Bundle/Tick records as JSONL). Committed so collaborators share state; taskstore's git merge driver resolves concurrent edits by timestamp.
- **Transient siblings under `.loopr/`** (logs, socket, pid, config, per-attempt worktrees) are ephemeral, machine-local, never committed. The target's `.git/info/exclude` carries the ignore patterns rather than `.gitignore` so loopr does not pollute the target's committed `.gitignore`. Patterns: `.loopr/runs/`, `.loopr/worktrees/`, `.loopr/socket`, `.loopr/daemon.pid`, `.loopr/config.yml`.
- **Per-attempt worktrees** (amended 2026-04-21; see a1 below) live at `.loopr/worktrees/<work-id>-<seq>/` — flat, inside the target, with a unique path-basename per attempt. Flat (not nested under `runs/<run-id>/`) because `git worktree add` derives the `$GIT_DIR/worktrees/<name>/` internal-registry folder name from the path basename; duplicate basenames don't hard-collide but trigger non-deterministic git-internal auto-suffixing that breaks reverse-mapping to our domain IDs. Provenance (which run created an attempt) is carried on the `Work` record in TaskStore, not in the filesystem path.
- **Upstream enablement.** `taskstore 0.5.0` split `open` into `open()` (default `$CWD/.taskstore/`) and `open_at(path, opts)` (exact path). The nested layout is consumer-controlled; `store::Store::open` passes `<target>/.loopr/taskstore/` to `open_at`.

### Worktree crash recovery

`Drop` guards clean up worktrees on the happy path but do not execute on SIGTERM, SIGKILL, or power loss. `loopr::daemon::startup::reconcile(target, store, live_sessions)` runs at daemon startup — **in the `loopr` binary, not in the `worktree` crate** (amended 2026-04-21, a3; the join between git state and Work-FSM transitions is orchestration, and `worktree` stays infrastructure-only). The routine:

1. `worktree::list(target, target.join(".loopr/worktrees"))` → `Vec<WorktreeInfo>` from `git worktree list --porcelain`, filtered by path prefix.
2. For each entry: `parse_branch(&info.branch)` → `(work_id, seq)` or skip (not a `loopr/wk-*` branch).
3. Join with TaskStore: `store.get_work(&work_id)` → `Some(work)` or log orphan and skip. Join with the daemon's in-memory `LiveSessions` map to check whether a coordinator is actively handling this `work_id`.
4. Dispose:
   - Work is terminal (`Done` / `Abandoned`): `worktree::cleanup_at(target, &info.path)`; if `Done`, also `worktree::delete_branch(target, &info.branch)` as belt-and-suspenders (integrator should have done it).
   - Work is non-terminal AND no live session: `store.mark_crash_interrupted(&work_id)`; leave worktree + branch for next attempt's disposal logic.
   - Work is non-terminal AND live session: noop, the session is handling it.
5. Orphaned `loopr/wk-*` branches with no corresponding worktree (manual cleanup drift, user intervention) are reported but not auto-deleted; human resolves.

No `.loopr/worktree-registry.jsonl` is read or written (amended 2026-04-21, a2; the file was redundant with git's own registry and added append-locking / corruption surface with no load-bearing value). Git's registry + the `loopr/wk-*` branch prefix + TaskStore is a complete three-way join.

Without this reconciliation, orphaned worktrees accumulate on disk and orphaned branches clutter the target's git history. Required by v5's reactive-daemon model; absent in v3/v4.

### Merge-driver limitation (inherited from taskstore)

`taskstore` ships a custom git merge driver that resolves `.loopr/taskstore/*.jsonl` conflicts by picking the record with the latest `updated_at` timestamp. `loopr init` installs the driver in `.git/config` on the local machine. **The driver only fires where installed.** Cloud-UI merges (GitHub web merge, GitLab, Gerrit) run on their servers without the driver; git's default textual merge on JSONL produces corrupt output when two branches edited the same record.

For v5 this risk is bounded — loopr's push policy is "never" (see "Git Posture"), so loopr-authored commits stay local. The risk surfaces when a human pushes `.loopr/taskstore/*.jsonl` across machines where loopr also runs. Mitigation is cultural: never resolve `.loopr/taskstore/*` merges via cloud UIs; always pull locally and merge on a machine where `loopr init` has run. Documented in full at `~/repos/scottidler/taskstore/docs/merge-driver-limitation.md`.

### CLI targeting: `-C <path>` (git-style)

- `loopr` uses **CWD** as the effective target by default.
- `loopr -C <path> <subcommand>` changes the effective target before any other logic runs, matching `git -C`.
- `LOOPR_TARGET` environment variable is a fallback when neither CLI nor CWD is definitive.

### Source-guard

`loopr`, before operating on an effective target, walks up from the target directory to `/` looking for the sentinel file `.loopr-source-guard`. If found, it refuses with a clear error. The sentinel is committed at the root of this loopr-v5 repo. Protects against v3/v4's recurring confusion where an agent treated the loopr source tree as the target.

### `loopr init` on a new target

Single idempotent command run inside the target (or via `-C`):

1. Create `.loopr/` and seed it with an empty `config.yml`.
2. Open the TaskStore via `Store::open(".")`, which creates `.loopr/taskstore/` on first call (path comes from `store::TASKSTORE_SUBPATH`).
3. Install taskstore's git hooks and merge driver (`taskstore install-hooks`).
4. Append the transient `.loopr/` subpaths (`runs/`, `socket`, `daemon.pid`, `config.yml`, `worktree-registry.jsonl`) to `.git/info/exclude` — leaving `.loopr/taskstore/` tracked — so the local loopr state is ignored without polluting the target's committed `.gitignore` and without excluding the committed truth.
5. Verify the target is not a loopr source tree (source-guard check).

## Prompts

Prompts are first-class content. Carrying over v3/v4's unfinished SSOT refactor (shared blocks like `SECTION_AC` live in one file, referenced elsewhere) with the themed directory structure v4 converged on.

### Layout

Prompts are baked into the binary via `include_dir!()` and written to `.loopr/prompts/` on `loopr init`. They live at the target-level (above `run-id`), so the user can edit them between runs and every run-id under that target picks up the edits. Themed directory structure preserved from v4:

```
.loopr/prompts/
├── agents/                 director.pmt, implementer.pmt, reviewer.pmt,
│                           researcher.pmt, tier-gate.pmt, interview.pmt
├── chat/                   default.pmt, draft.pmt, executing.pmt, ... (TUI-era)
├── decompose/              grouped by hierarchy level
│   ├── plan/, spec/, phase/, work/
│   └── strategies/         strategy selection (.yml)
├── engine/                 fsm/, strategies/, triggers/
└── partials/               SSOT chunks referenced by name
    ├── section-ac.pmt
    ├── section-context.pmt
    └── ...
```

Extension: `.pmt` (matches v4; signals "prompt" over "handlebars template" so we can swap engines later without a mass rename).

### Templating

**`handlebars-rust`** (5.x). Logic-less matches v5's "no magic" thesis. Partials map SSOT cleanly: `{{> section-ac}}` pulls from `partials/section-ac.pmt`. Direct serde integration: pass a `Serialize` struct as context, reference fields with `{{work.id}}`, `{{#each acs}}`. Dynamic Rust content via registered helpers when the logic-less bent bites.

### Versioning and telemetry

Prompts version with code; no prompt-semver scheme. The `telemetry` crate captures the fully-rendered prompt on every LLM-call span and attaches it to the produced `Bundle` record, so a single run is fully reproducible from its own log. Past-run replay across prompt bumps is deferred; AutoResearch picks that up when it earns its keep.

### Golden-file tests

None for now. Accept drift; revisit if a prompt change silently breaks a working behavior.

### Overrides — three-layer resolution

Resolution order (first hit wins):

1. `.loopr/prompts/<path>` (per-target override)
2. `~/.config/loopr/prompts/<path>` (user-level baseline)
3. baked-in via `include_dir!()` (fallback)

Rationale: user-level baseline satisfies DRY across multiple target repos (e.g., "always prefer `eyre` over `unwrap`" lives at the user layer once, not duplicated per target). Per-target override stays supreme when a specific repo has unique needs (e.g., `rust-version` wants a different tone than `python-scraper`). Baked-in always exists as the guaranteed fallback.

`loopr init` seeds `.loopr/prompts/` only with files the user explicitly chose to override (via a future `loopr prompts edit <path>` command) or leaves it empty. The three-layer fallback resolves paths at runtime.

## Error Model

Typed end-to-end. The v5 thesis — typed seams, not string dispatch — applies to error paths too.

### Tool errors

`Tool::run` returns `Result<Output, ToolError>`. `ToolError` is a **closed** typed enum; all variants typed; some variants may carry a `String` for detail; **no `eyre::Report` escape hatch**. Retry strategies pattern-match on variants; when a case doesn't fit an existing variant, add one.

### Agent failure persistence

When a ralph loop aborts mid-Work, the `Bundle` / `Work` record stores a typed `FailureReason` enum **plus** a companion `error: String` side field for human-readable detail. Variants: `TokenBudget`, `ToolFailure { tool: String }`, `ReviewerRejection`, `AcUnmet`, `Panic`, `Other(String)`. Downstream counters (`bundle_rejections`, `attempt_count`, `session_failure_count` — the doom-loop safety nets from v4) match on these variants structurally.

### Panic posture

Every agent task is wrapped in `catch_unwind`. A panic becomes `FailureReason::Panic` on the Bundle; the daemon stays up. No scenario where a single Work panicking takes the daemon down with it.

### RPC errors in `ipc`

Closed Rust enum on the daemon/client sides, serialized to JSON-RPC-compatible `{code, message}` on the wire. v4's error codes carry over (`-32601`/`-32602`/`-32603` standard + `-32000` through `-32005` loopr-specific). Matching on the Rust side stays typed; the wire stays JSON-RPC-standard.

### User-facing surface

- **One-shot subcommands** (`loopr --version`, `loopr plan "..."` when the call is short): eyre-formatted errors to stderr.
- **Long-running operations and the daemon itself**: emit `DaemonEvent::Error { run_id, plan_id, work_id, reason, message }` on the event stream. Pre-TUI clients subscribe to the stream and render errors as they arrive; TUI, when it lands, puts them in a panel.

## Models and Budgets

Models are tiered by config, pinned-at-call-time by telemetry, and bounded by per-scope budgets.

### Role-to-model mapping

Top-level `models:` block with named tiers. Roles reference tiers by name **or** accept literal model IDs. Deserializer tries the tier table first, falls back to literal.

```yaml
models:
  primary: claude-sonnet-4-7
  lightweight: claude-haiku-4-5
  advisor: claude-opus-4-7

agents:
  implementer: { model: primary }           # tier reference
  reviewer:    { model: primary }
  tier-gate:   { model: claude-haiku-4-5 }  # literal, also accepted
```

Swapping model versions across the whole system = one line in the `models:` block. AutoResearch variant YAMLs override sparsely.

### Pinning discipline

Config holds **floating** tags (`claude-sonnet-4-7`). Every LLM call records the **concrete model ID** the API returned on its telemetry span **and** on the produced `Bundle` record. Config stays readable; logs and records stay auditable. v4 burned on silent model-ID changes mid-run; this pattern removes that failure mode.

### Budgets

Per-Work cap + per-run cap. Per-role caps earned later when a specific role runs away. Enforcement: **soft pause only**. Hitting either cap stops new agent spawns, lets in-flight agents finish, notifies the client with a `DaemonEvent::BudgetExceeded`. Resume requires explicit user action. No hard kill — in-progress Bundles are never discarded.

### Cost audit

Append-only `.loopr/costs.jsonl`, one line per LLM call:

```json
{"ts": "...", "run_id": "...", "plan_id": "...", "work_id": "...",
 "role": "implementer", "model": "claude-sonnet-4-7-20260115",
 "input_tokens": 1234, "output_tokens": 567, "cost_usd": 0.042}
```

`loopr costs` queries this directly (trivial awk/jq consumer); full trace context lives in the same events on the telemetry stream.

### Config override chain

For all config knobs (not just API keys), resolved in this order (later wins):

**baked-in defaults < XDG user config (`~/.config/loopr/loopr.yml`) < target config (`.loopr/config.yml`) < environment variable < CLI flag.**

The target config sits above the XDG user config: repo-specific overrides trump user-wide defaults. Env and CLI still trump everything for one-shot invocations.

Naming transformation:
- Config keys: `lowercase-separated-by-hyphens` in YAML.
- Env vars: `LOOPR_` prefix + `ALL_CAPS` + `-` → `_` (`log-level` → `LOOPR_LOG_LEVEL`, `models.primary` → `LOOPR_MODELS_PRIMARY`).
- CLI flags: same as the YAML key (`--log-level`).

**Why the `LOOPR_` prefix** (reversing an earlier no-prefix decision after Architect Round 3): loopr spawns subprocesses via `tools` (cargo, npm, bash, ...). Any env var present in loopr's process is inherited by the child unless explicitly scrubbed. A bare `LOG_LEVEL=trace` intended for loopr would leak into every subprocess and affect build behavior non-reproducibly; `PORT` or similar generic names are worse. The `LOOPR_` prefix namespaces loopr's knobs away from the universe of env vars that spawned subprocesses might care about.

API keys specifically: loopr honors standard SDK env vars (`ANTHROPIC_API_KEY`) in addition to the chain above. Config-file storage is allowed with a `# only safe on non-shared machines` warning in the template; keychain integration is deferred.

## Git Posture

Agents make commits. Rules settled early so habits form correctly.

### Author identity

Your `~/.gitconfig` identity. Agent commits look like yours in `git blame` for now. Revisit if mixing human and agent work becomes hard to disentangle.

### Signing

Inherited from your git config. Since the agent commits under your identity, `user.signingKey` flows through automatically. You already sign; agent commits are signed without loopr doing anything special.

### Trailers

Every agent commit carries structured trailers:

```
Loopr-Run: 20260418-123653
Loopr-Plan: <plan-id>
Loopr-Work: <work-id>
Loopr-Role: Implementer
Loopr-Model: claude-sonnet-4-7-20260115
```

No `Co-Authored-By: Claude` trailer. `git log --grep="Loopr-Work: <id>"` traces a commit back to its full run context.

### Push policy

**Never.** All branches stay local to the target. Ticks land on the integration branch in the target's local git; you review and push manually when ready. Revisit once v5 actually builds something worth pushing.

### Branch naming

All loopr-owned refs share a `loopr/` prefix, distinguishing them from human branches and from v4's bare `agent/wk-*`:

- **Per-Plan integration branches:** `loopr/plan-<plan-id>`
- **Per-Work agent worktree branches:** `loopr/wk-<work-id>`

## Security

Agents execute code. The Bash tool is the largest blast radius; bwrap contains it.

### Sandbox posture: the `security.sandbox` knob

A three-value config knob controls how strict sandbox enforcement is. Default is `required` so the secure path is the path of least resistance; `preferred` and `off` exist for environments where `bwrap` is genuinely unavailable:

| Value | Behavior | When to use |
|---|---|---|
| `required` (default) | `loopr init` fails cleanly if `bwrap` is absent, with install instructions. Every `Local`-lane tool runs under `bwrap --unshare-net`. | Dev laptops, desktops, any long-running environment. Matches the discipline that v4 intended but lost. |
| `preferred` | If `bwrap` is present, use it (same behavior as `required`). If absent, emit a prominent `tracing::warn!` at startup and every Local-lane tool invocation; run the tool unsandboxed. Startup proceeds. | Corporate / shared hosts where installing bubblewrap is impossible. Known-unsafe but explicit. |
| `off` | Skip sandbox entirely, even if `bwrap` is present. No warnings. | CI (GitHub Actions, GitLab CI, Docker-based runners) where the container itself is the isolation boundary. AutoResearch harness running in CI sets this. |

Rationale for the knob (replacing v4's silent-fall-back "preferred"-style default): the user makes the compromise explicitly per target, at init time. `loopr init` prints the chosen posture and the detected `bwrap` status so there's no ambiguity about what will run. v4's failure mode was that the unsandboxed path was reached silently via warnings; here, reaching `preferred` or `off` requires a deliberate config edit.

Install: `apt install bubblewrap` on Debian/Ubuntu; distro equivalent otherwise. macOS has no direct equivalent; macOS users default to `preferred` with a documented note in `docs/vision.md` that macOS runs unsandboxed.

### Lane model (three-lane, v4 verbatim)

Each tool classifies into a lane. Lane determines sandbox posture, concurrency, and timeouts:

| Lane | Network | Sandbox | Slots | Default timeout | Max timeout | Use |
|---|---|---|---|---|---|---|
| `Local` | blocked | `bwrap --unshare-net` | 10 | 30s | 60s | filesystem tools (read, write, edit, list, tree, glob, grep, find) |
| `Net` | allowed | none | 5 | 60s | 120s | network tools (fetch, search, shell) |
| `Heavy` | allowed | none | 1 (serialized) | 600s | 1800s | builds, tests, lints (cargo test, otto ci, npm test, configured project tools) |

Tool-to-lane classification is a straight string match on tool name. Unknown tools default to `Heavy` (conservative: slot-limit + long timeout).

### Denylist

A base hardcoded denylist of obvious footguns fails Bash commands fast before subprocess launch:

- `rm -rf /`, `rm -rf ~`, `rm -rf $HOME`
- `sudo *`
- `curl ... | sh`, `wget ... | sh`
- `git push` (pushes anywhere; push policy is human-only)
- `gh repo delete`

Target-level extension via `.loopr/config.yml`: targets can add project-specific denies (e.g., forbid an agent from running `deploy.sh`).

### Target overrides

**Tighten-only.** `.loopr/config.yml` can add denies, force stricter lane rules, reduce slot limits. It **cannot** widen permissions or disable the sandbox. Prevents a target from silently downgrading to "off" without a user action they feel.

## Process Rules

The rules that change the failure mode of v1–v4, not the rules that the architecture already enforces:

1. **One design doc at a time, motivated by a failing run.** No new detailed doc until the previous one has produced a passing E2E against a real target repo. `docs/vision.md` is the exception - it's the seed.
2. **Seam tests, not only unit tests.** Every crate boundary has at least one golden-file test: given this input serialized, produce this output. Serde round-trip tests cover `deny_unknown_fields`. Unit tests on one side of a seam are not enough.
3. **No coexistence migrations.** If a stage needs to change, change it in place or replace it in one commit. No dual paths. No "both systems run during migration."
4. **One architectural pivot per quarter, at most.** And only after three consecutive passing E2Es against real projects. v3/v4's four paradigm shifts in 55 days is the exact tempo not to repeat.
5. **CLI is plumbing, TUI is porcelain.** A CLI verb must have a real user story outside an interactive UI (scripts, cron, CI, git hooks, one-off shell invocations). Interactive management flows (retry, approve, reject, inspect-with-context, live-watch) live in the TUI as keybindings, not in the CLI as verbs. Added by amendment `a6`; see `docs/design/2026-04-23-cli-plumbing-shape.md` for the rationale and the verb surface.

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
- **Fsm derive:** revive `#[derive(Fsm)]` from v3's `loopr-derive` (v4 deleted it in favor of a runtime YAML interpreter that v5 also rejects). Rehomed in `crates/derive` under v5's single-word naming convention. The v5 shape is v3's compile-time derive plus v4's rich-error output minus v4's YAML/string-dispatch regressions; full rationale and provenance in [`docs/design/2026-04-20-fsm-macro.md`](design/2026-04-20-fsm-macro.md). New `#[derive(Record)]` joins it; derives only, no function-like or attribute macros.
- **Tool registry:** minimal for first gate ({Read, Write, Edit, Bash, Grep, Glob}). Expands toward Claude / Gemini / Codex parity as TUI Chat and agent capability needs earn each addition.
- **Observability:** `tracing` + `tracing-subscriber` + `tracing-appender`, owned by `crates/telemetry`. Overrides the `rules/rust.md` default of `log` + `env_logger` for v5 specifically.
- **Target-repo state:** single top-level `.loopr/` with `.loopr/taskstore/` (committed truth) and everything else transient (excluded via `.git/info/exclude`).
- **Prompts:** handlebars-rust, `.pmt` files, themed layout under `.loopr/prompts/` populated from `include_dir!()`, partials for SSOT, **three-layer override resolution** (target → XDG user → baked-in). See "Prompts" section.
- **Error model:** closed typed `ToolError` enum; typed `FailureReason` + companion `error: String` on records; `catch_unwind` at every agent task; closed RPC enum serializing to JSON-RPC wire codes; dual user-surface (stderr for one-shot, `DaemonEvent::Error` stream for long-running). See "Error Model" section.
- **Models and budgets:** tiered `models:` block; floating config, pinned telemetry; per-Work + per-run caps, soft pause only; `.loopr/costs.jsonl`. Config override chain: baked-in < XDG < `.loopr/config.yml` < env var < CLI flag; env vars use a `LOOPR_` prefix + `ALL_CAPS` to avoid polluting spawned subprocess environments. See "Models and Budgets" section.
- **Git posture:** user's git identity, user's signing config, rich `Loopr-*` trailers (no `Co-Authored-By: Claude`), never push, `loopr/plan-<id>` and `loopr/wk-<id>` branch prefixes. See "Git Posture" section.
- **Security:** three-lane model (`Local`/`Net`/`Heavy`) verbatim from v4; base denylist of footguns + tighten-only target overrides; **sandbox posture as a `security.sandbox: required | preferred | off` knob**, defaulting to `required`, explicit downgrade required to run unsandboxed. See "Security" section.
- **Crate restructure (Architect rounds 1+2+3):** `runtime` junk-drawer split into `store`/`llm`/`tools`/`worktree`. `domain` stripped to records+FSM only (source-level; transitive purity pending taskstore-traits). `integrator` dep graph enforces no-LLM rule at Cargo level. `context` extracted as a shared prompt-assembly crate (Round 3 fix: `decomposer` and `agents` both call LLMs; they share `context` instead of duplicating). `tools` stripped of `domain` dep (IDs flow via spans from `telemetry`, not via typed struct fields). Workspace is 13 crates. See "Crate Layout".
- **`ContextBuilder` placement:** lives in the `context` crate, which is depended on by both `decomposer` and `agents`. Earlier placements (in `llm`, then in `agents`) were superseded. `context` sees `domain` + `store` + `tools` + `telemetry`; `llm` consumes the Messages it produces.
- **`agents` dependency injection:** `Deps<L, T, W, S, C>` struct pattern (one generic parameter bundles all trait bounds) instead of spreading generics across every function signature. Mitigates the Architect Round 3 ergonomic concern while respecting `rules/rust.md`'s no-`dyn` rule.
- **`LOOPR_` env var prefix (restored):** env vars are `LOOPR_<KEY>` (all caps, hyphens to underscores). Earlier bare-ALL_CAPS decision reversed after Architect Round 3 flagged subprocess env inheritance as a correctness hazard: `tools` spawns cargo/npm/bash, which inherit loopr's environment, so unprefixed `LOG_LEVEL` would pollute child processes.
- **IPC framing:** NDJSON via `tokio_util::codec::LinesCodec` with 1 MiB max line; JSON-RPC-style envelope (`id`/`method`/`params`, `id`/`result`/`error`, unsolicited `event`/`data`); error codes `-32601`/`-32602`/`-32603` standard + `-32000`–`-32005` loopr-specific. All verbatim from v3/v4. See "ipc" ABI section.
- **Worktree crash recovery:** daemon startup routine `loopr::daemon::startup::reconcile(target, store, live_sessions)` joins `git worktree list --porcelain` + `loopr/wk-<id>-<seq>` branch-name parse + TaskStore, cleans orphans, marks crashed Works as `FailureReason::CrashInterrupted`. See "Worktree crash recovery" subsection. Amended 2026-04-21 (a2/a3): registry JSONL removed; reconcile moved from `worktree` crate to `loopr` binary.
- **Taskstore merge-driver limitation:** inherited from `taskstore`; documented here, full write-up in `~/repos/scottidler/taskstore/docs/merge-driver-limitation.md`. Mitigation is cultural (never merge `.loopr/taskstore/*` via cloud UIs).
- **Upstream `taskstore-traits` split** — shipped. `scottidler/taskstore` is now a two-crate workspace (release `v0.3.0`): `taskstore-traits v0.1.0` holds `Record`/`IndexValue`/`Filter`/`FilterOp` with only `serde` as a dep; full `taskstore` re-exports them and layers on the Store engine. loopr-v5 wires both as git deps in `[workspace.dependencies]`; `domain` pulls trait-only, `store` pulls the full crate. `domain`'s transitive-dep purity is now structurally enforced.
- **Inter-crate Cargo-level dep wiring** — was pending at Round 3 review (`[dependencies]` blocks were empty); landed in the follow-up commit that declares `path = "../..."` deps for every internal edge of the 13-crate graph. No longer open after that commit.

**Open:**
- New questions get added here when they surface.

## Amendments

This section logs post-publication amendments to the vision. Each entry is dated, identifies which section(s) above were edited, names the design doc that drove the change, and summarizes the rationale. Amendments are applied in-place to the sections above — this log exists so the history of decisions is scannable without `git log`-archaeology. A design doc that amends the vision MUST add an entry here.

Convention: each amendment gets an `aN` identifier (a1, a2, a3, ...) that the in-text amendment callouts reference. IDs are append-only and monotonic across all amendment dates.

### 2026-04-21 — Worktree lifecycle (Stage 7)

Design doc: [`docs/design/2026-04-21-worktree-lifecycle.md`](design/2026-04-21-worktree-lifecycle.md)
Reviewed by: Architect R1 (pre-draft) + R2 (post-draft).
Sections edited: "worktree" crate contract, Target Repo Layout tree, Rationale for nesting taskstore, Worktree crash recovery, Closed Open Questions.

- **a1 — Worktree layout: sibling → flat interior.** Was `<target-parent>/<target-name>-work-<work-id>/`. Now `<target>/.loopr/worktrees/<work-id>-<seq>/`. Why: sibling paths fail in CI checkout layouts, containers, and read-only parent mounts; break atomic cleanup when the target is deleted; pollute the user's workspace namespace. Flat interior is collision-safe with git's internal worktree registry (which auto-disambiguates by path basename in non-deterministic ways when duplicates appear).
- **a2 — `.loopr/worktree-registry.jsonl` removed.** Git's own worktree registry (`git worktree list --porcelain`) + `loopr/wk-*` branch-name prefix + TaskStore is a complete, race-free reconcile path. The JSONL added a third source of truth with append-locking (`fcntl`/`fs2`), partial-write corruption recovery, and fold-forward terminal-marking semantics — all to surface data git already exposes structurally.
- **a3 — Reconcile moved from `worktree` crate to `loopr` binary.** Was `worktree::reconcile(target)`. Now `loopr::daemon::startup::reconcile(target, store, live_sessions)`. Why: reconcile joins git state with TaskStore + `Work`-FSM mutations + live-session map; that's cross-crate orchestration, which is the `loopr` binary's job. `worktree` stays infrastructure-only (primitives: `list`, `cleanup_at`, `delete_branch`, `parse_branch`, `ensure_loopr_excludes`).
- **a4 — Per-attempt fresh worktree with monotonic `<seq>`.** New: every Work attempt gets its own worktree at `<work-id>-<seq>/` on branch `loopr/wk-<work-id>-<seq>`. Seq is allocated internally by `Worktree::create` via an EEXIST-retry loop using git's own "already exists" stderr phrases (`LC_ALL=C` forced for locale stability; exit code 128 explicitly NOT used as a retry signal). No branch reuse, no rebase-on-retry, no commit preservation. v4's "NO-OP loop" retry trauma (commits `0ce1226` → `120c29b`) is made structurally impossible because there is no reused branch to carry rejected commits forward.
- **a5 — `AttemptCleanupPolicy` enum.** New: four variants (`Immediate` / `OnWorkTerminal` / `OnRunEnd` / `Never`), default `OnWorkTerminal`, exposed via config.yml + ENV (`LOOPR_WORKTREE_CLEANUP_POLICY`) + CLI (`--worktree-cleanup`), precedence CLI > ENV > config > default. Debugging-oriented toggle for "why did the implementer produce garbage" investigations: flip to `OnRunEnd` or `Never` to preserve the forensic artifact without a code change. `Never` is strict debug-only - it leaks memory + file descriptors on long-uptime daemons and requires a restart to clear accumulated state. *Amended by `a6`: the CLI flag was never wired through to config loading (dead code) and was deleted; precedence is now ENV > config > default.*

### 2026-04-23 - CLI plumbing shape

Design doc: [`docs/design/2026-04-23-cli-plumbing-shape.md`](design/2026-04-23-cli-plumbing-shape.md)
Reviewed by: Architect R1 (direct-reads hazard) + R2 (frame-cap + prefix-literal correction).
Sections edited: `loopr` crate bullet (§Crate Layout ABI Contracts), Process Rules (added #5), Amendments log.

- **a6 - Process Rule #5 "CLI is plumbing, TUI is porcelain" added.** A CLI verb must have a real user story outside an interactive UI. Interactive management flows (retry/approve/reject/inspect-with-context/live-watch) live in the TUI as keybindings. Rationale: without this rule, every future CLI addition argues from scratch; with it, the question is always "what's the scripting case?" and an absent answer is the answer.
- **a6 - CLI surface reduced to 9 plumbing verbs + bare TUI launch.** Deleted `decompose` / `execute` / `integrate` / `score` / `list <kind>` (architectural zombies under the auto-chain daemon model; they would never gain bodies). Added flat list verbs `plans` / `works` / `bundles` / `ticks`, detail `show <id>` (2-char prefix routed), and TUI launcher (bare `loopr` or explicit `loopr tui`, currently returning `NotYetImplemented` until the TUI crate ships).
- **a6 - `--output {json|yaml}` global with TTY auto-default.** New `-o` / `--output` global flag. Default chosen from `std::io::IsTerminal`: YAML when stdout is a TTY (human-readable), JSON when piped (script-friendly). Explicit flag always wins.
- **a6 - IPC adds `record.list` / `record.get` with `RecordKind` enum; `plan.list` deleted.** One pair of generic methods replaces per-type list methods. Adjacent-tagged sum-type results (`RecordsResult { Plans(Vec<PlanSummary>) | Works(...) | ... }`, `RecordResult { Plan(Plan) | ... }`). Adding Spec/Phase later is two `RecordKind` variants plus two arms per result enum - no new methods.
- **a6 - `record.list` returns lightweight summary projections, not full records.** `PlanSummary` / `WorkSummary` / `BundleSummary` / `TickSummary` carry id + key parent(s) + status + updated_at only (~200-400 bytes each JSON-encoded). Full records go through `record.get`. Rationale: 1 MiB `MAX_LINE_BYTES` frame cap would otherwise break `loopr bundles` at mature-repo scale. Frame-size regression test pins 1000 `BundleSummary` under 512 KiB.
- **a6 - CLI reads go through the daemon, not direct taskstore.** The CLI links `ipc` for reads (`RecordList` / `RecordGet`) rather than `store` directly. Architect R1 flagged that direct-disk reads would instantiate a second `AsyncStore` concurrent with the daemon's (SQLite write contention, read-after-write inconsistency, silent `.loopr/taskstore/` auto-creation on uninitialized targets). One seam for state; direct filesystem only for non-state artifacts (`.loopr/runs/*.log` reads for `logs tail` / `logs runs`).
- **a6 - `--worktree-cleanup` CLI flag deleted as dead code.** It was clap-parsed but never consumed by `Config::load` or the daemon context constructor; the env + config precedence was already doing all the work. The precedence in `a5` above is amended to `ENV > config > default`.
- **a6 - `[Stage N]` markers dropped from subcommand help text.** Teaching-tool artifacts from the Stage 1 skeleton; now clutter.
- **a6 - Collection-type naming rule added to `rules/rust.md`.** Naming and Style §"Collection type names: plural-s, not 'List' suffix" - prefer `Plans`, `RecordsResult`, `Bundles` over `PlanList`, `RecordListResult`, `BundleList`. Applies across struct names, enum variants, and IPC method/result names.

### 2026-04-24 - Record field naming sweep + testability note

Design doc: follow-up cleanup to [`docs/design/2026-04-23-cli-plumbing-shape.md`](design/2026-04-23-cli-plumbing-shape.md).
Sections edited: Amendments log (this entry); no structural vision sections.

- **a7 - Rename compound `*_sha` field names to plain `sha`.** `domain::Tick::integration_sha` -> `sha`; `ipc::TickSummary::integration_sha` -> `sha`; `Worktree::base_sha()` accessor + private field -> `sha()` / `sha`; `Worktree::create(..., base_sha)` parameter -> `sha`; `worktree::resolve_base_sha` public fn -> `resolve_sha`. Plus the `agents::dispatch::compute_loc_changed(path, base_sha)` parameter and assorted locals / test fixtures. Rationale: the struct and function context already disambiguates (a `Tick.sha` can only be the integration-branch head; a `Worktree.sha()` can only be the base the worktree branched from), so the qualifier carried no information. Local variables in scopes with two simultaneously-live SHAs (e.g., `integrator::git::verify_branch` had `base_sha` and `head_sha`, `integrator::lib::integrate_core` had `pre_merge_sha` and `integration_sha`) were disambiguated by dropping `_sha` entirely (`base`/`head`, `pre_merge`/`sha`), not by keeping the compound.
- **a7 - Rename `domain::Tick::integration_branch` field to `branch`.** Same principle as `_sha`. Function names that describe the-thing-they-create (`daemon::git::ensure_integration_branch`, test helper `init_repo_with_integration_branch`) and error variants (`IntegrationError::IntegrationBranchMissing`) kept their full names - those are semantically distinct from the field and benefit from the qualifier.
- **a7 - Document the `resolve_inner` testability pattern.** The Stage-1 design doc text called for a small `OutputStream` indirection trait to make `Format::resolve`'s TTY auto-detect testable. Implementation instead factored out `fn resolve_inner(explicit: Option<Format>, stdout_is_tty: bool) -> Format` (pure fn over an injected bool). Tests pass explicit `true`/`false`; the public wrapper calls `std::io::stdout().is_terminal()`. Same testability at less surface area. Design doc amended to describe what was actually built.
