# Design Document: Tier 1 cleanup - close every "claimed-but-not-built" gap

**Author:** Scott Idler
**Date:** 2026-04-25
**Status:** Implemented (Phases 1-10); Phase 11 e2e gate deferred — see "Phase 11 — deferred" below
**Crates touched:** loopr, store, agents, decomposer, integrator, llm, telemetry, worktree
**Review Passes Completed:** 4/5 (Architect R4 folded 2026-04-25)

## Summary

`docs/three-tiers-of-broken-implementation.md` enumerates seven Tier 1 items where the codebase claims (in `CLAUDE.md` files, doc comments, or design docs) that something is done, when in fact the work is partial, missing, or wired in name only. This doc is the single plan that closes all seven, in dependency order, with one commit per phase, otto ci green at every phase boundary, executable end-to-end via `/how-to-execute-a-plan`.

## Problem Statement

### Background

Each of the seven Tier 1 items is a different shape of "lying-by-omission":

1. **`loopr init` scope mismatch.** `crates/loopr/CLAUDE.md` describes five jobs (`.loopr/`, TaskStore, hooks, `.git/info/exclude`, source-guard verify); the implementation does one (seeds `.loopr/prompts/`).
2. **`LooprError` dead variants.** `StageUnimplemented` and `NotYetImplemented` (`crates/loopr/src/error.rs:16-17, 46-47`) exist only to gate work that "will be earned later." Their two callers are an unreachable dispatch arm and a deferred-feature placeholder.
3. **Decomposer transcripts not wired.** `telemetry::transcript::append_iteration` ships and is called by Implementer + Reviewer; `decomposer::decompose` does not call it. `plans/<plan-id>/decomposition.md` is never written.
4. **System-prompt elision documented but not built.** Iterations 2..N in the Implementer Ralph loop re-render and re-send the full system prompt, paying full input-token cost on each iteration. Anthropic's `cache_control` is unused.
5. **Per-record summaries: writers exist, callers don't.** `summary::write_work` / `write_plan` / `write_bundle` ship; only `BundleUpdateSink` exists; per-transition fanout at the spawn methods is missing. Summaries appear only on Integrator success.
6. **Process / session digests not built.** The XDG layout reserves `runs/<pid>/summary.md` and `sessions/<sid>/summary.md`; nothing writes them.
7. **Stale dead-code allowances + unwired `StoreError::Corruption`.** `transition_and_persist_bundle` and `transition_and_persist_plan` carry `#[allow(dead_code)]` markers that are out of date (both functions are now called). `StoreError::Corruption` is genuinely unused.

The shared root cause: rule #1 of the Working Rules ("one design doc at a time, motivated by a failing run") was treated as license to ship partial implementations under a final-sounding name and defer the rest indefinitely. Rule #1 has been removed; this doc is the catch-up batch.

### Problem

Seven user-visible or operator-visible gaps:

| # | What the user sees |
|---|---|
| 1 | `loopr init` against a fresh target leaves the target unable to run a daemon without further setup. |
| 2 | `loopr daemon start` exposes a `StageUnimplemented` error that is structurally unreachable; bare `loopr` and `loopr tui` produce a generic "not yet implemented" with no clear next step. |
| 3 | A weird decomposition (one Work for a multi-criterion goal) leaves no transcript to debug from. |
| 4 | Implementer Ralph loops cost ~4-5x more in input tokens than they need to. |
| 5 | `<target>/.loopr/records/works/<id>/summary.md` and `plans/<id>/summary.md` are stale or absent until the Integrator runs. |
| 6 | A daemon exit (graceful or panic) leaves no human-readable rollup of what happened. |
| 7 | Corrupt JSONL surfaces as `Serde(...)`, indistinguishable from a schema mismatch; stale `#[allow(dead_code)]` trains future readers to leave suppressions in place. |

### Goals

- Every Tier 1 item from the inventory resolved.
- One commit per phase; otto ci green after each.
- No half-states left behind in `main` (or `v5`); every phase is shippable on its own merits.
- Tests added in the phase that introduces the behavior, not deferred.
- The CLAUDE.md "Wiring status:" notes that exist today and are honest about gaps get updated to reflect reality after each phase lands.

### Non-Goals

- Tier 2 items: missing v3 action types (`read_file`, `write_file`, `create_learning`), Researcher/Director agents, multi-tier decomposition (Plan/Spec/Phase/Work).
- Tier 3 items: integrator validation execution, work-only crash recovery, `Plan.decomposition_attempts`, `LlmError::Retryable` Duration carry, TUI, e2e success-pattern automation, prompt cache mtime invalidation, AutoResearch CLI verbs, lane configuration ergonomics.
- Bootstrapping a non-git target. The source-guard already requires a git repo.
- Generating a target `config.yml`. Tracked separately.
- New `TranscriptIteration` fields. Existing struct is reused.
- Performing summary writes inside OCC transactions. Best-effort posture is preserved.
- Auto-recovery from `Corruption` (rebuild SQLite cache from JSONL, repair JSONL from a backup). Detection only; recovery is a follow-up.
- Multi-tier prompt caching breakpoints (caching tools or messages independently of system).

## Proposed Solution

### Overview

Seven targeted fixes, ordered by blast radius from smallest (LooprError variants) to largest (loopr init refactor + e2e gate). Each item gets a compact spec below; the Implementation Plan section is the executable phase list.

### 1. `loopr init` completion

`Init::run` becomes the orchestrator of six idempotent steps:

```
1. verify_source_guard(target)        - re-confirm + print outcome
2. create_loopr_dir(target)           - mkdir -p .loopr/
3. open_taskstore(target)             - Store::open creates .loopr/taskstore/ on first call
4. install_taskstore_hooks(target)    - in-process via taskstore::hooks::install
5. ensure_git_excludes(target)        - delegate to worktree::ensure_loopr_excludes
6. seed_prompts(target, force)        - existing seeder (unchanged)
```

Each step returns `StepOutcome { Created | Preserved | Skipped { reason } }`. Idempotent: re-running on a fully-initialized target prints "preserved" lines and exits 0. Failures are categorized (fatal vs recoverable); recoverable failures emit `warn!` and continue.

### 2. `LooprError` dead variants

```rust
// REMOVE from crates/loopr/src/error.rs
StageUnimplemented { stage: u8, subcommand: &'static str }   // line 16-17
NotYetImplemented { feature: &'static str }                  // line 46-47
```

```rust
// ADD
#[error("the TUI is not built into this binary; install the `loopr-tui` crate or run `loopr <subcommand>`")]
TuiNotInstalled,
```

Migrate the two callers:

- `DaemonCmd::Start { .. } => unreachable!("DaemonCmd::Start is fork-hoisted in run() before dispatch")` (`crates/loopr/src/lib.rs:181`).
- `Command::Tui => Err(LooprError::TuiNotInstalled)` (`crates/loopr/src/lib.rs:198`).

Update `crates/loopr/src/tests.rs` to expect `TuiNotInstalled`.

### 3. Decomposer transcripts

In `crates/decomposer/src/decompose.rs`, after each `try_llm_once` and at every error return, build a `TranscriptIteration` and call `telemetry::transcript::append_iteration(decomposer_path(target, &plan.id), &iter)`. Mapping:

| `TranscriptIteration` field | Source |
|---|---|
| `iteration` | 1 for initial call, 2 for retry |
| `system_prompt` | `assemble_system(...)?` result |
| `user_prompt` | `first_user` or `retry_user` |
| `response` | `tool_call.input` re-serialized to JSON |
| `parsed_actions` | rendered `DecomposeResponse` summary (one line per child: `<title> (deps: [...]) ac=<n>`) |
| `outcome` | the same string already passed to `span.record("outcome", ...)` |
| `started_at`, `latency_ms` | computed at call boundaries |

Best-effort: a transcript-write failure emits `warn!` and the decomposer continues.

### 4. System-prompt elision (Anthropic prompt caching)

`AnthropicClient` builds the `system` field as an array with one block carrying `cache_control: { "type": "ephemeral" }`:

```rust
fn build_system_block(system: &str) -> serde_json::Value {
    serde_json::json!([{
        "type": "text",
        "text": system,
        "cache_control": { "type": "ephemeral" }
    }])
}
```

#### Precondition: token-count measurement

Anthropic prompt caching has a model-dependent minimum cacheable threshold: 1024 tokens for Haiku, 2048 tokens for Sonnet 4.x and Opus 4.x. A request with `cache_control` on a block below the threshold succeeds but the cache silently no-ops; `cache_creation_input_tokens` stays at zero, `cache_read_input_tokens` stays at zero, and the projected savings never materialize.

Phase 4 starts by measuring the assembled system prompt token count for each role (Implementer, Reviewer, Decomposer) using the model's tokenizer (`tiktoken`-equivalent or Anthropic's `count_tokens` API). Outcomes:

- **All three prompts above 2048 tokens (Sonnet/Opus minimum):** proceed with the design unchanged; commit message records the measured sizes.
- **Implementer below 2048 but Decomposer/Reviewer above:** still proceed (Implementer is the iteration-heavy role; Decomposer + Reviewer are single-shot, the savings there are bonus). Commit message records actual savings expectation.
- **Implementer below 2048:** stop. Either bulk the system prompt (more tool schemas, more guardrail copy, role description) until it crosses the threshold, or accept that this phase ships zero cost reduction and document the outcome. The decision is the user's; the doc author flags the measurement and waits.

The measurement step lands as a one-line note in the commit body of Phase 4 (`measured: implementer=<N> tokens, reviewer=<N>, decomposer=<N>`). It does not become a CI gate.

#### `LlmClient` trait widening (cross-cutting prerequisite)

This phase changes the `LlmClient` trait surface. Today both methods discard `Usage`:

```rust
// crates/llm/src/client.rs (current)
fn complete_with_tool<'a>(...) -> impl Future<Output = Result<ToolCall, LlmError>> + Send + 'a;
fn complete_free<'a>(...) -> impl Future<Output = Result<String, LlmError>> + Send + 'a;
```

Phase 4 widens both to return `Usage` alongside the existing payload:

```rust
fn complete_with_tool<'a>(...) -> impl Future<Output = Result<(ToolCall, Usage), LlmError>> + Send + 'a;
fn complete_free<'a>(...) -> impl Future<Output = Result<(String, Usage), LlmError>> + Send + 'a;
```

`Usage` is defined in `crates/llm/src/usage.rs` (new file):

```rust
#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)] pub cache_creation_input_tokens: u64,
    #[serde(default)] pub cache_read_input_tokens: u64,
}

impl Usage {
    pub fn cache_hit_ratio(&self) -> f64 { ... }
}
```

Re-exported from `crates/llm/src/lib.rs` so `agents`, `decomposer`, and `telemetry` can name it.

**Blast radius — every call site changes.** Verified call sites today:

- `crates/agents/src/implementer.rs:194, 336` (`complete_free`)
- `crates/agents/src/reviewer.rs:227` (`complete_free`)
- `crates/decomposer/src/decompose.rs:180` (`complete_with_tool`, via `try_llm_once` helper)
- Every `impl LlmClient for MockLlm` in tests (`crates/agents/src/{implementer,reviewer}/tests.rs`, `crates/decomposer/src/decompose/tests.rs`, anywhere else `grep -r "impl LlmClient" crates/` finds).

Migration pattern at each call site:
```rust
// before
let raw = deps.llm.complete_free(&assembled.system_prompt, &messages).await?;
// after Phase 4 — discard Usage; the call site never reads it directly
let (raw, _usage) = deps.llm.complete_free(&assembled.system_prompt, &messages).await?;
```

Phase 4 makes the trait change and updates every site to discard `Usage` (`let (x, _usage) = ...`). Phase 7 then wraps the underlying `LlmClient` with `MeteredLlmClient<L>` at daemon boot; that wrapper destructures the tuple, increments `ProcessSnapshot` counters, and forwards the payload, so the call sites continue to use `let (x, _usage) = ...` unchanged. The wrapper is the sole counter — no per-call-site `record_llm_call` invocations get added in Phase 7. Mock impls in tests update once (in Phase 4) and stay updated.

Both `cache_creation_input_tokens` and `cache_read_input_tokens` are recorded on `llm.anthropic` and `llm.anthropic.free` spans. A DEBUG event `llm.anthropic.cache` carries `created`, `read`, `cache_hit_ratio`.

### 5. Per-record summaries (Work + Plan)

Two new traits in `crates/store/`, mirroring `BundleUpdateSink`:

```rust
pub trait WorkUpdateSink: Send + Sync {
    fn update<'a>(&'a self, work: Work) -> impl Future<Output = Result<(), WorkUpdateError>> + Send + 'a;
}

pub trait PlanUpdateSink: Send + Sync {
    fn update<'a>(
        &'a self,
        plan: Plan,
        children: Vec<Work>,    // option (c): caller fetches children before update
    ) -> impl Future<Output = Result<(), PlanUpdateError>> + Send + 'a;
}
```

Real impl on `Store` (the impl ignores `children` for persistence; it only persists `plan` via `PlansStore::update`). Forwarding impl for `&S`. Errors `WorkUpdateError::Update(String)` and `PlanUpdateError::Update(String)`.

**Why option (c-extended) for `PlanUpdateSink` + `WorkUpdateSink`:** writing a Plan summary requires the children Works (`store.works().list_by_parent_id(&plan.id)` → `summary::write_plan(target, plan, &children)`). A pure decorator over `PlanUpdateSink` has no read access. Plain option (c) — pushing the children fetch to the Plan-update caller — keeps the `PlanUpdateSink` impl trait-pure, but leaves the on-disk Plan summary stale across child-Work transitions (only `WorkUpdateSink` fires for those, and that impl writes only the Work summary). Option (c-extended) closes that freshness gap by giving `SummaryFanout` an `Arc<Store>` handle that the `WorkUpdateSink` impl uses to fetch the Work's parent Plan and its children, re-rendering the Plan summary on every Work transition. The trait-purity goal of plain c is preserved at the `PlanUpdateSink` boundary; the concrete-store coupling is scoped to the Work-update side where it is genuinely needed. See Alternatives Considered §4 for the plain-c-vs-c-extended tradeoff.

A `SummaryFanout<S>` decorator in `crates/loopr/src/daemon/summary_fanout.rs` implements all three sink traits when the inner type does, writing the matching summary on every successful update. The Plan path receives `children` as a parameter and forwards directly into `summary::write_plan(target, &plan, &children)`. The Work path additionally fetches the parent Plan + its children and refreshes the Plan summary; a missing-or-non-Plan parent is a `debug!` skip. Failed summary write logs `warn!` and returns `Ok(())`.

`transition_and_persist_work`, `transition_and_persist_bundle`, `transition_and_persist_plan` become sink-generic. `transition_and_persist_plan` additionally takes `children: Vec<Work>` so the call site fetches children before invoking the helper. `DaemonContext` holds a `SummaryFanout<Arc<Store>>` and threads it through every spawn method. Existing inline `write_*_summary_best_effort` calls at the integrator's success path are deleted; the fanout handles them.

### 6. Process / session digests

`ProcessSnapshot` accumulates counters during the daemon's lifetime, held inside `DaemonContext` as `Arc<Mutex<ProcessSnapshot>>`:

```rust
pub struct ProcessSnapshot {
    pub started_at: SystemTime,
    pub plans_created: u32,
    pub works_created: u32,
    pub works_completed: u32,
    pub works_blocked: u32,
    pub bundles_proposed: u32,
    pub bundles_accepted: u32,
    pub ticks_created: u32,
    pub llm_calls: u32,
    pub llm_input_tokens: u64,
    pub llm_output_tokens: u64,
    pub llm_cache_read_tokens: u64,
    pub llm_cost_usd_micros: u64,
    pub escalations: u32,
    pub abnormal_exit: Option<String>,
}
```

A `MeteredLlmClient<L>` wrapper increments LLM counters after each call, with a per-model rate table in `crates/telemetry/src/digest/cost.rs`.

`render_process_digest(&ProcessSnapshot) -> String` produces YAML frontmatter + markdown body. `write_process_digest` is called from `serve_core`'s post-loop tail and from a `std::panic::set_hook` (with reentrancy guard via `AtomicBool`). A SIGQUIT handler writes the digest before forced exit.

`render_session_digest` walks `sessions/<sid>/targets/*/runs/*/summary.md`, parses each frontmatter, builds `SessionAggregate`, renders. Called from `loopr sessions end` after `session::end_active` succeeds.

### 7. Stale `#[allow(dead_code)]` + `StoreError::Corruption`

Remove `#[allow(dead_code)]` from:

- `transition_and_persist_bundle` (`crates/loopr/src/daemon/context.rs:749`); call site at `daemon/startup.rs:234`.
- `transition_and_persist_plan` (`crates/loopr/src/daemon/context.rs:773`); call site at `daemon/context.rs:643`.

Wire `StoreError::Corruption` at the JSONL read boundary. Edit the `From<taskstore_async::Error>` impl in `crates/store/src/error.rs:48`:

```rust
impl From<taskstore_async::Error> for StoreError {
    fn from(e: taskstore_async::Error) -> Self {
        match e {
            taskstore_async::Error::Serde(inner) => match inner.classify() {
                serde_json::error::Category::Syntax | serde_json::error::Category::Eof => {
                    StoreError::Corruption(inner.to_string())
                }
                _ => StoreError::Serde(inner.to_string()),
            },
            other => StoreError::Io(other.to_string()),
        }
    }
}
```

Remove `#[allow(dead_code)]` from the `Corruption` variant.

#### Daemon-boot policy on corruption (explicit)

Skipping a corrupt JSONL row at boot and continuing silently is dangerous: the daemon ends up with a partial view of records and may make FSM transitions, allocate Work ids, or write Bundles that conflict with the corrupt-but-still-on-disk row when it is later repaired. Failing fast is the inverse extreme and prevents an operator from booting a daemon at all to triage the damage.

The chosen policy: **skip-and-refuse-to-listen by default, with an explicit operator override.**

1. **Per-record skip with structured error.** When `sweep_worktrees` or `sweep_bundles` encounters `Err(StoreError::Corruption(_))`, it emits `tracing::error!(file = ..., id = ..., "corrupt record skipped during sweep")` and increments `DaemonContext::corruption_count: AtomicUsize`. The sweep continues with the next record. This matches the existing `RecordNotFound` skip pattern (`crates/loopr/src/daemon/startup.rs:178, 211`).
2. **Post-sweep gate.** After `reconcile` returns, the daemon checks `corruption_count`. If non-zero AND `--accept-corruption` was not passed, the daemon refuses to bind the IPC listener and exits non-zero with an actionable message:
   ```
   daemon refused to start: <N> corrupt record(s) detected during sweep.
     Logs:    loopr -C <target> logs tail
     Restore: git -C <target> checkout HEAD -- .loopr/taskstore/  (taskstore is JSONL+SQLite-as-cache; JSONL is git-tracked)
     Override: re-run with --accept-corruption to start in degraded mode
   ```
   Exit code: a new `LooprError::CorruptionGate { count: usize }` mapped to a stable non-zero CLI exit.
3. **Operator override.** `loopr daemon start --accept-corruption` flips the gate from "refuse" to `tracing::warn!("starting with N corrupt records skipped")` and proceeds. The flag is daemon-scoped; it does not persist past the daemon's lifetime.
4. **Summary surface.** `corruption_count` becomes a field on `ProcessSnapshot` (Phase 7), so per-process digests record the boot's corruption tally even when the override was used.

Why not fail-fast unconditionally: an operator must be able to boot a daemon to see `loopr logs tail`, inspect the corrupt record's surrounding state, and decide on `git restore` vs manual repair. Refusing the IPC listener while still producing the structured error and a valid exit code is the middle ground: read tools work (because they don't need the daemon), the client-mode CLI works, only the daemon refuses to attach until corruption is resolved.

Why not silent-skip: a partial-state boot that runs Implementer or Integrator against a worldview missing records is the exact "bug eats the user's repo" failure mode this codebase exists to avoid.

## Implementation Plan

Eleven phases, one commit per phase. Each phase ends with `otto ci` at the workspace root and (when the phase touches more than one crate) per-crate `otto ci` inside each touched crate. Commit messages follow Conventional Commits.

### Phase 1: `LooprError` cleanup
**Model:** sonnet
**Touches:** `crates/loopr/`

Tasks:
- Delete `StageUnimplemented` and `NotYetImplemented` from `crates/loopr/src/error.rs`.
- Add `LooprError::TuiNotInstalled`.
- Replace `DaemonCmd::Start { .. } => Err(StageUnimplemented {...})` with `unreachable!("...")` at `crates/loopr/src/lib.rs:181`.
- Replace `Command::Tui => Err(NotYetImplemented {...})` with `Err(LooprError::TuiNotInstalled)` at `crates/loopr/src/lib.rs:198`.
- Update test at `crates/loopr/src/tests.rs:84-98` to expect `TuiNotInstalled`.
- Strike the comment at `crates/loopr/src/tests.rs:37` that references `Phase 5 replaces that stub`.
- Update doc comments at `crates/loopr/src/transport/client.rs:94` and `crates/loopr/src/cli.rs:41` that name the deleted variants.

Tests: existing tests updated; no new tests required.

Verify: `otto ci` at the workspace root.

Commit: `chore(loopr): remove dead StageUnimplemented and NotYetImplemented variants`

### Phase 2: Dead-code allowances + corruption gate via `taskstore::list_tolerant`
**Model:** opus
**Touches:** `crates/loopr/`, `crates/store/`, `Cargo.toml` (workspace)

**Prerequisite shipped:** taskstore workspace v0.6.1 (tag `v0.6.1`) ships:

- `taskstore::Store::list_tolerant<T: Record>(&self, filters: &[Filter]) -> Result<ListResult<T>, taskstore::Error>` (sync).
- `taskstore_async::AsyncStore::list_tolerant<T: Record>(&self, filters: &[Filter]) -> Result<ListResult<T>, taskstore::Error>` (async via `tokio::task::spawn_blocking`; bypasses both writer thread and reader pool).

JSONL-direct read; bypasses SQLite cache; tombstones filtered. Types live in `taskstore-traits`:

- `ListResult<T> { records: Vec<T>, corruption: Vec<CorruptionEntry> }` (`#[non_exhaustive]`, `::new(records, corruption)` constructor).
- `CorruptionEntry { file: PathBuf, line: u64, raw: String, error: CorruptionError }`.
- `CorruptionError::{InvalidJson { msg, category }, MissingId, TypeMismatch { msg }, Io { kind }}`.
- `Category::{Syntax, Eof, Data, Io}` — taskstore-owned mirror of `serde_json::error::Category`.

The workspace also unifies error handling on `taskstore::Error` (the per-crate `eyre::Result` was dropped in v0.6.0). v0.6.1 incidentally fixed `FilterOp::Contains` to do real substring matching (binds `'%value%'`); reconcile uses no filters today, so this doesn't affect us.

Tasks:
- Bump the workspace `taskstore`, `taskstore-async`, and `taskstore-traits` deps to tag `v0.6.1` in root `Cargo.toml` (the flat workspace tag).
- Remove `#[allow(dead_code)]` from `transition_and_persist_bundle` (`crates/loopr/src/daemon/context.rs:749`).
- Remove `#[allow(dead_code)]` from `transition_and_persist_plan` (`crates/loopr/src/daemon/context.rs:773`).
- Add `Store::list_tolerant_<kind>(...)` thin wrappers in `crates/store/src/{bundles,works}.rs` (one per Record type the daemon sweeps: `Bundle`, `Work`). Each wrapper forwards to `taskstore_async::AsyncStore::list_tolerant<T>(filters)` and maps `Result<ListResult<T>, taskstore::Error>` to `Result<ListResult<T>, StoreError>` via the existing `From<taskstore::Error>` impl. Re-export `ListResult`, `CorruptionEntry`, `CorruptionError`, `Category` from `crates/store/src/lib.rs` so reconcile code names them via `store::*`.
- Replace `ctx.store.bundles().list().await` in `sweep_bundles` (`crates/loopr/src/daemon/startup.rs:208`) with `ctx.store.list_tolerant_bundles(&[]).await?`. Iterate `result.records` for the requeue logic; merge `result.corruption.len()` into `ReconcileReport::corruption_count`.
- Replace `sweep_worktrees`'s per-id `store.works().get(work_id)` lookup pattern (`crates/loopr/src/daemon/startup.rs:178`) with a `ctx.store.list_tolerant_works(&[]).await?` pre-pass that materializes the live Work set; merge `result.corruption.len()` into `ReconcileReport::corruption_count` alongside the bundle pass. Reason: a JSONL-malformed Work record never reaches SQLite (taskstore's `sync()` silently drops it), so `get(work_id)` returns `Ok(None)` and the existing skip-as-missing path leaves the gate's `corruption_count` at 0 for corrupt Works. The `list_tolerant_works` pass is what surfaces them. The existing per-id `get` path is kept after the pre-pass for the worktree-to-Work matching logic, since that uses parsed branch names rather than a flat scan.
- For each `CorruptionEntry` in either result (bundles or works), emit `tracing::error!(file = %entry.file.display(), line = entry.line, error = ?entry.error, "corrupt record skipped during sweep")`.
- Modify `ReconcileReport` (`crates/loopr/src/daemon/startup.rs:47`) to carry `corruption_count: usize`.
- Add `LooprError::CorruptionGate { count: usize }` variant; map to a stable non-zero CLI exit code in `main.rs`'s exit-mapper.
- Add `--accept-corruption` flag to `DaemonCmd::Start` (`crates/loopr/src/cli.rs`); default `false`.
- Wire the post-sweep gate in `crates/loopr/src/daemon.rs` between `reconcile` returning and `bind_listener` being called: if `report.corruption_count > 0 && !accept_corruption`, return `LooprError::CorruptionGate { count }` with the actionable error message text from the "Daemon-boot policy on corruption" subsection. Otherwise emit `tracing::warn!("starting with N corrupt records skipped")` and proceed.

Tests:
- Integration: `crates/store/tests/corruption.rs` exercises the wrappers — write a JSONL with one valid + one truncated row, assert `list_tolerant_bundles` returns `ListResult` with `records.len() == 1, corruption.len() == 1`.
- Integration: synthetic daemon against a target with a corrupt JSONL, assert daemon refuses to listen and exits with `CorruptionGate { count: 1 }`.
- Integration: same target plus `--accept-corruption`, assert daemon starts with the warn and `corruption_count` propagated to `ProcessSnapshot` (re-verified after Phase 7).

Verify: `otto ci` workspace + per-crate.

Notes for the executor:
- We do not need `StoreError::Corruption` wiring at the `From<taskstore::Error>` layer anymore. taskstore's `list_tolerant` returns the corruption metadata directly as data, not as an `Err`. Drop that earlier subtask.
- v0.6.0 unified the workspace error type: `taskstore_async::Error` is gone; the canonical type is `taskstore::Error` (re-exported from `taskstore_async::Error` for compat or used directly). Update `crates/store/src/error.rs`'s `From<taskstore_async::Error>` impl to `From<taskstore::Error>` accordingly.

Commit: `feat(store, loopr): wire taskstore list_tolerant for corruption-aware sweep with --accept-corruption override`

### Phase 3: Decomposer transcripts
**Model:** sonnet
**Touches:** `crates/decomposer/`

Tasks:
- Add `write_decomposer_transcript` helper in `crates/decomposer/src/decompose.rs`, modeled on `write_implementer_transcript` from `crates/agents/src/implementer.rs:43-65`.
- Add `render_decompose_response(&DecomposeResponse) -> String`: one line per child, `<title> (deps: [...]) ac=<n>`.
- Wrap `try_llm_once` with started_at / latency_ms capture.
- Call `write_decomposer_transcript` after the success path's parse, with `outcome = "ok"`.
- Call at every error return inside `decompose`: `LlmFailed`, `MalformedChildren`, `ZeroChildren`, `EmptyTitle`, `DuplicateTitles`, `CycleDetected`, `UnresolvedDeps`, `EmptyAcceptanceCriteria`. Skip for `collect_workspace_tree` failure (no LLM call yet).
- Best-effort: write failure emits `warn!`, decomposer continues.

Tests:
- `crates/decomposer/src/decompose/tests.rs`: `transcript_written_on_success`, one test per validation-error variant, `transcript_written_with_two_iterations_on_retry`, `transcript_skipped_on_workspace_scan_failure`.

Verify: `otto ci` workspace + `crates/decomposer/`.

Commit: `feat(decomposer): wire append_iteration so plan decomposition transcripts land on disk`

### Phase 4: Anthropic prompt caching + `LlmClient` trait widening
**Model:** opus
**Touches:** `crates/llm/`, `crates/agents/`, `crates/decomposer/`

This phase carries two coupled changes: (1) wire `cache_control: ephemeral` on the system prompt; (2) widen the `LlmClient` trait to return `Usage` so Phase 7's `MeteredLlmClient` has data to consume. Both must land in one commit because the trait change is observable from every call site, and shipping (1) without (2) would force a re-touch of every site in Phase 7.

Tasks (in order):

1. **Token-count measurement (informational).** Render the assembled system prompt for Implementer, Reviewer, Decomposer using current `PromptLoader` against a representative target. Count tokens via Anthropic's `count_tokens` API (or `tiktoken` approximation). Record measurements in the commit body as `measured: implementer=<N>, reviewer=<N>, decomposer=<N>`. **Decision is locked: proceed regardless of outcome.** If a prompt sits below the 2048-token Sonnet/Opus minimum, the cache wiring no-ops on that role — accepted. Do not artificially bulk the prompt to cross the threshold. The trait-widening + cache wiring lands either way; if Anthropic lowers thresholds or the prompt grows naturally later, the win materializes without further work.
2. **Define `Usage`.** New file `crates/llm/src/usage.rs` with `Usage { input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens }`, all `u64`, the cache fields `#[serde(default)]`. `pub fn cache_hit_ratio(&self) -> f64`. Re-export from `crates/llm/src/lib.rs`.
3. **Widen the trait.** Change `LlmClient::complete_with_tool` to return `Result<(ToolCall, Usage), LlmError>` and `LlmClient::complete_free` to return `Result<(String, Usage), LlmError>`. Update the forwarding `impl<L: LlmClient + ?Sized> LlmClient for std::sync::Arc<L>` accordingly.
4. **Update `AnthropicClient`.** Stop discarding `Usage` from the response. Build the `system` field via the new `build_system_block(system: &str) -> serde_json::Value` helper carrying `cache_control: { "type": "ephemeral" }`. Record `cache_creation_input_tokens`, `cache_read_input_tokens`, `cache_hit_ratio` on `llm.anthropic` and `llm.anthropic.free` spans. Emit `tracing::debug!("llm.anthropic.cache", created, read, ratio)` after each successful response.
5. **Update every call site.** Add `let (raw, _usage) = ...` (or destructured equivalent) at:
   - `crates/agents/src/implementer.rs:194` and `:336`.
   - `crates/agents/src/reviewer.rs:227`.
   - `crates/decomposer/src/decompose.rs:180` (inside `try_llm_once`; ripples up to `decompose`'s call site).
   - Any other site `grep -rn "complete_free\|complete_with_tool" crates/` reveals.
6. **Update every `MockLlm` test fake.** Add `Usage::default()` to every return value. Sites: `crates/agents/src/implementer/tests.rs`, `crates/agents/src/reviewer/tests.rs`, `crates/decomposer/src/decompose/tests.rs`, `crates/agents/tests/seam_implementer_then_reviewer.rs`, `crates/integrator/src/tests.rs`, anywhere else `grep -rn "impl LlmClient" crates/` finds.
7. **Live integration smoke.** `crates/llm/tests/cache_smoke.rs` gated `#[ignore]`: two back-to-back `complete_free` calls with the same system prompt against a real `ANTHROPIC_API_KEY`; asserts `cache_read_input_tokens > 0` on the second response.

Tests:
- Update existing mock-server tests to assert the new request shape (`system` is an array, the block carries `cache_control`).
- Unit: `Usage` deserialization for both old (no cache fields) and new (with cache fields) response shapes.
- Compile-only: every call-site update is verified by `cargo check --workspace`.

Verify: `otto ci` workspace + every touched crate. Run `cargo test -p llm --test cache_smoke -- --ignored` manually with `ANTHROPIC_API_KEY` set; record observed cache hit ratio in the commit body.

Commit: `feat(llm): widen LlmClient to return Usage; enable Anthropic prompt caching on system prompts`

### Phase 5: `WorkUpdateSink` + `PlanUpdateSink` + `SummaryFanout`
**Model:** opus
**Touches:** `crates/store/`, `crates/loopr/`

Tasks:
- Add `WorkUpdateSink`, `WorkUpdateError`, real impl on `Store`, forwarding impl for `&W`. Mirror `BundleUpdateSink`'s shape from `crates/store/src/bundles.rs:170-235`. Method signature: `fn update<'a>(&'a self, work: Work) -> impl Future<...>`.
- Add `PlanUpdateSink`, `PlanUpdateError`. Method signature: `fn update<'a>(&'a self, plan: Plan, children: Vec<Work>) -> impl Future<...>`. The real impl on `Store` ignores `children` for persistence (it only updates the `plans` collection via `PlansStore::update`); `children` exists for the decorator's summary-render path. `Plan` updates today are not OCC-tracked; sink omits `expected_updated_at`.
- Re-export both traits + error types from `crates/store/src/lib.rs`.
- Add `crates/loopr/src/daemon/summary_fanout.rs`. `SummaryFanout<S>` wraps an inner sink, the target path, and an `Arc<Store>` (needed for the c-extended Work-update path that re-renders the parent Plan summary). Implement all three sink traits conditional on which the inner satisfies. On `Ok(())` from the inner:
  - `WorkUpdateSink`: call `summary::write_work(target, &work)`. **C-extended addition:** if `work.parent_id` resolves to a Plan, fetch the parent (`store.plans().get(&parent_id)`) and that Plan's children (`store.works().list_by_parent_id(&parent_id)`) and call `summary::write_plan(target, &plan, &siblings)` so the on-disk Plan summary reflects the child-Work transition. A missing parent (e.g. parent_id points at a Spec/Phase rather than a Plan, or the parent has been deleted) emits `debug!` and skips silently — Plan-summary refresh is best-effort.
  - `BundleUpdateSink`: call `summary::write_bundle(target, &bundle)`.
  - `PlanUpdateSink`: call `summary::write_plan(target, &plan, &children)` — `children` is the param the caller passed in.
  - Any failure emits `warn!` and the impl returns `Ok(())` so a summary error never rolls back a successful FSM transition.

**Why c-extended over plain c:** plain option (c) leaves the on-disk Plan summary stale until the Plan itself transitions — so a Plan whose children walk `Pending → InProgress → InReview → Done` shows status counts that lag the actual child state. C-extended makes the Plan summary refresh on every child-Work transition so the on-disk view is always current. Cost is one extra `plans().get` and one `works().list_by_parent_id` per Work transition; both are cheap reads against the in-process store. The asymmetry with `BundleUpdateSink` (which does not refresh anything else) is honest: bundles do not have a child-of-Plan structure that the Plan summary depends on.

Tests:
- Sink trait fakes (`MockWorkSink`, `MockBundleSink`, `MockPlanSink`) + `SummaryFanout<MockSink>` integration tests. The `MockPlanSink` test asserts the decorator passes `children` through unchanged.
- New: c-extended Work-update path — drive a Work whose `parent_id` resolves to a known Plan, assert both `<work-id>/summary.md` AND `<plan-id>/summary.md` are written, and assert the Plan summary's status counts reflect the new Work state.
- New: c-extended degenerate path — drive a Work whose `parent_id` does not resolve to a Plan (or the parent has been deleted), assert only `<work-id>/summary.md` is written and the missing-parent path emits `debug!` (not `warn!`).
- Failure injection: read-only target dir, assert each sink's `update` returns `Ok(())` and the `warn!` is emitted.

Verify: `otto ci` workspace + `crates/store/` + `crates/loopr/`.

Commit: `feat(store, loopr): add Work and Plan update sinks plus SummaryFanout decorator`

### Phase 6: Wire `SummaryFanout` at every transition site
**Model:** opus
**Touches:** `crates/loopr/`, `crates/integrator/`

Tasks:
- Make `transition_and_persist_work`, `transition_and_persist_bundle` generic over their sink trait (`&S where S: WorkUpdateSink`, `&S where S: BundleUpdateSink`) instead of taking `&Store`.
- Make `transition_and_persist_plan` sink-generic AND extend its signature to take `children: Vec<Work>` (option (c)). At every `transition_and_persist_plan` call site, fetch children with `store.works().list_by_parent_id(&plan.id).await?` immediately before the helper call. The integrator's plan-completion path at `crates/loopr/src/daemon/context.rs:643` already needs children for the summary, so the read is not new work — it just moves to the call site.
- Add a `summary_fanout: SummaryFanout<Arc<Store>>` field to `DaemonContext`. Construct it in `DaemonContext::new`, passing the same `Arc<Store>` both as the inner sink AND as the c-extended store handle (the decorator uses the inner for the write-through, the store handle for the parent-Plan refresh read).
- Update every `transition_and_persist_*` call site in `crates/loopr/src/daemon/context.rs` and `daemon/startup.rs` to pass `&self.summary_fanout` (or the appropriate sink reference). For Plan call sites, also fetch and pass `children`.
- Update the integrator's `BundleUpdateSink` injection at the daemon's spawn site to pass the fanout decorator.
- Delete the now-unused `write_bundle_summary_best_effort`, `write_work_summary_best_effort`, `write_plan_summary_best_effort` functions and their call sites at `crates/loopr/src/daemon/context.rs:661-664`.

Tests:
- Existing seam tests (`crates/agents/tests/seam_implementer_then_reviewer.rs`) updated to inject a `SummaryFanout<MemStore>` and assert summaries exist after the seam runs.
- New: drive one `Pending -> InProgress -> InReview` transition through a synthetic daemon, assert `<target>/.loopr/records/works/<id>/summary.md` exists and contains the expected status.
- New: drive a `Bundle::Reviewed` transition, assert the bundle summary exists.
- Existing integrator happy-path test continues to pass; assert all three summaries exist after merge.

Verify: `otto ci` workspace + every touched crate.

Commit: `feat(loopr): wire SummaryFanout at every Work/Bundle/Plan transition`

### Phase 7: `ProcessSnapshot` plumbing + per-process digest at exit
**Model:** opus
**Touches:** `crates/telemetry/`, `crates/llm/`, `crates/loopr/`

Tasks:
- Add `crates/telemetry/src/digest/process.rs` with `ProcessSnapshot` struct, `inc_*` helpers, `render_process_digest`, `write_process_digest`.
- Add `crates/telemetry/src/digest/cost.rs` with the per-model rate table; cost stored in `u64` micros.
- Add `Arc<Mutex<ProcessSnapshot>>` field to `DaemonContext`. Add `corruption_count: usize` field to `ProcessSnapshot`; populate it from `ReconcileReport::corruption_count` (Phase 2 added the source).
- Increment counters at the major transition points: plan create, work create / status changes, bundle propose / accept, tick create, escalations.
- Wrap the existing `LlmClient` with `MeteredLlmClient<L>` that updates `ProcessSnapshot::llm_*` after each call. The trait already returns `Usage` (Phase 4); the wrapper destructures the tuple, increments counters, then forwards the payload. Construct the metered wrapper at daemon boot.
- **Counter ownership transfer.** Phase 4's call-site updates landed `let (raw, _usage) = ...` everywhere — the `_usage` discard is the placeholder for Phase 7's wiring. Phase 7 does NOT add per-call-site `record_llm_call(&usage)` invocations at the implementer / reviewer / decomposer call sites; instead, the `MeteredLlmClient<L>` wrapper at daemon boot is the sole counter. The call sites stay as `let (raw, _usage) = ...` (the wrapper has already counted the call by the time the destructured `usage` reaches the caller). Stated explicitly so future readers don't add a second counting site.
- Wire `write_process_digest` into `serve_core`'s post-loop tail (`crates/loopr/src/daemon.rs:506`): clone the snapshot, drop the lock, render, atomic-write.
- Install `std::panic::set_hook` in `serve_core` setup that sets `abnormal_exit = Some(message)` and writes the digest. Use `AtomicBool` reentrancy guard.
- Install SIGQUIT handler that triggers digest write before forced exit.

Tests:
- Per-counter increment tests in `digest/process/tests.rs`.
- Renderer golden-file test for the YAML frontmatter + markdown body shape.
- Integration: synthetic daemon (using existing test harness scaffolding), submit a plan, drive one Work to Done, send SIGTERM, assert per-process digest exists with expected counters.

Verify: `otto ci` workspace + every touched crate.

Commit: `feat(telemetry, loopr): per-process digest at graceful and abnormal exit`

### Phase 8: Session digest at `loopr sessions end`
**Model:** sonnet
**Touches:** `crates/telemetry/`, `crates/loopr/`

Tasks:
- Add `crates/telemetry/src/digest/session.rs` with `SessionAggregate`, walker that parses each `runs/<pid>/summary.md` frontmatter, `render_session_digest`, `write_session_digest`.
- Frontmatter parser handles missing/unparseable per-process digests gracefully (warn line in body, skip file, continue).
- Wire `digest::session::write_session_digest(&id)` into `crates/loopr/src/commands/sessions.rs::end` after `session::end_active(target)?` returns `Some(id)`.

Tests:
- Per-aggregator unit test: synthesize three per-process digests under a tempdir, run the aggregator, assert rolled-up counters.
- Integration: run two synthetic daemons against the same session id, end the session, assert session digest aggregates both.

Verify: `otto ci` workspace + every touched crate.

Commit: `feat(loopr): write session digest at sessions end with per-process aggregation`

### Phase 9: `loopr init` completion
**Model:** opus
**Touches:** `crates/loopr/`

Tasks:
- Refactor `crates/loopr/src/commands/init.rs` into the six-step shape. Move existing `seed_prompts` body to be the last step.
- Add `StepOutcome { Created { detail }, Preserved { detail }, Skipped { reason } }` and `InitReport`.
- Make `Init::run` async; plumb the `await` through the dispatcher in `crates/loopr/src/lib.rs`.
- Implement the four new steps:
  - `verify_source_guard(target)`: re-run `crate::guard::check_source_guard(target)` and report.
  - `create_loopr_dir(target)`: `std::fs::create_dir_all(target.join(".loopr"))`. Detect existing-vs-new via `try_exists`.
  - `open_taskstore(target)`: `store::Store::open(target).await`. Detect first-call vs subsequent via `target.join(store::TASKSTORE_SUBPATH).try_exists()`.
  - `install_taskstore_hooks(target)`: invoke `Store::install_git_hooks(&self) -> Result<()>` (instance method on the open Store; see `taskstore` v0.2.3 `src/store.rs:734`). Detect first-call vs already-installed by checking the canonical hook paths (`.git/hooks/{pre-commit,post-commit,...}`) before invoking; if all expected hooks are already present, report `Preserved`.
  - `ensure_git_excludes(target)`: delegate to `worktree::ensure_loopr_excludes(target)`. Detect "added" vs "already present" by comparing `.git/info/exclude` line counts before and after.
- Print one human-readable line per step plus a final "init complete" totals line.
- Update `crates/loopr/src/cli.rs:51-59` doc comment to describe the actual behavior; delete the "Future scope:" sentence.
- Update `crates/loopr/CLAUDE.md` "loopr init:" bullet if anything has shifted.
- Update `docs/vision.md` "loopr init on a new target" section to match the new step list.

Tests:
- Per-step unit tests using `tempfile::TempDir` as a synthetic git repo.
- Fresh-target end-to-end: assert all six steps return `Created`.
- Already-initialized end-to-end: assert all six return `Preserved` or `Skipped { reason: "already present" }`.
- Failure injection: read-only `.loopr/`, missing `.git/info/`, sentinel-tripped target.

Verify: `otto ci` workspace + `crates/loopr/`.

Commit: `feat(loopr): loopr init now performs full target setup (.loopr, taskstore, hooks, excludes, prompts)`

### Phase 10: Documentation rollup
**Model:** sonnet
**Touches:** `docs/`, every crate's `CLAUDE.md`

Tasks:
- `crates/loopr/CLAUDE.md`: rewrite the "Wiring status:" lines for transcripts (Decomposer now wired), summaries (per-transition fanout now wired), and process/session digests (now built). Update the "loopr init:" bullet to match Phase 9.
- `crates/store/CLAUDE.md`: mention `WorkUpdateSink` and `PlanUpdateSink` alongside `BundleUpdateSink`. Document `StoreError::Corruption` semantics (corrupt JSONL vs schema mismatch) and the `git restore` recovery path.
- `crates/agents/CLAUDE.md`: document the cache-locality contract for the system prompt: "the system prompt MUST be byte-stable across iterations within a single Ralph loop. Iteration-specific state goes into the user message, not the system."
- `crates/llm/CLAUDE.md`: extend "Instrumentation" to mention `cache_creation_input_tokens` and `cache_read_input_tokens`.
- `docs/design/2026-04-22-stage-8-wiring.md` (Phase 8.5 section): add a "Shipped: WorkUpdateSink, PlanUpdateSink, SummaryFanout in 2026-04-25-tier1-cleanup.md" line.
- `docs/design/2026-04-24-prompts-on-disk.md` Post-Implementation Findings: add lines for decomposer transcripts shipped + system-prompt elision shipped.
- `docs/design/2026-04-24-loopr-layout.md`: add a "Shipped: per-process digest, session digest" section.
- `docs/vision.md` "Four-layer log strategy": rename to "Five-layer log strategy" and add the digest layer.

Tests: none. Documentation only.

Verify: `otto ci` workspace (catches markdown link rot via `whitespace -r`).

Commit: `docs: update CLAUDE.md and design notes after Tier 1 cleanup`

### Phase 11: E2E gate
**Model:** opus
**Touches:** test artifacts only

Tasks:
- Run `bin/e2e --build rust-version` against the standard target.
- Assert observable outcomes:
  - `loopr init` against a fresh target produces every artifact (`.loopr/`, `.loopr/taskstore/`, `.loopr/prompts/`, hooks installed, `.git/info/exclude` populated).
  - `loopr daemon start` succeeds; no `StageUnimplemented` ever surfaces.
  - Decomposition produces a `plans/<plan-id>/decomposition.md` transcript.
  - First implementer iteration writes the transcript; iteration 2 hits the cache (`cache_read_input_tokens > 0` in spans).
  - Per-record summaries appear at every transition: `works/<id>/summary.md` exists at `Pending -> InProgress`, refreshes at `InReview`, and final at `Done`.
  - Per-process digest exists at `runs/<pid>/summary.md` after `loopr daemon stop`.
  - Session digest exists at `sessions/<sid>/summary.md` after `loopr sessions end`.
  - No `#[allow(dead_code)]` warnings or surprises in `cargo build`.
- If any check fails, fix the relevant phase and re-run.
- Record results in the commit body (sample digest paths, observed cache-hit ratio, transition counts).

Tests: the e2e itself.

Verify: e2e exits clean; `otto ci` workspace stays green.

Commit: `test(e2e): Tier 1 cleanup gate against rust-version target`

#### Phase 11 — deferred

Phases 1-10 shipped as planned. Phase 11 (e2e gate) is **deferred** to a follow-up run. Rationale: the implementation work — every Tier 1 item from the inventory — is closed, with `otto ci` green at every commit boundary and the per-phase unit + integration tests covering the new code surface. The e2e gate is verification, not implementation; running it requires `ANTHROPIC_API_KEY` budget and 5–30 minutes of real-LLM execution against the `rust-version` target. The user opted to ship the closure now and run the e2e gate as a follow-up. Tracked separately so it doesn't block release.

Verification now: the per-phase test suites + workspace `otto ci` are the gate. The e2e adds shape-level assertions (digest paths exist, transcripts land on disk, cache_read_input_tokens > 0 on iteration 2) that are not regressed by anything in Phases 1-10's test scope.

## Alternatives Considered

### Alternative 1: Seven separate design docs, executed independently
- **Description:** What the previous draft of this work tried. One design doc per Tier 1 item.
- **Pros:** Smaller individual reviews; easier to merge in any order.
- **Cons:** Seven PRs, seven review cycles, seven version bumps, seven CLAUDE.md churn passes. Cross-cutting concerns (the dead-code allowances span loopr + store; the prompt-caching telemetry spans llm + telemetry; the summary fanout spans store + loopr + integrator) get artificially split. The `/how-to-execute-a-plan` skill expects ONE plan, not seven.
- **Why not chosen:** The user explicitly redirected toward consolidation; seven separate docs is a process tax with no reviewer benefit at this size.

### Alternative 2: Defer some items to Tier 2 and ship a smaller batch
- **Description:** Drop process/session digests (Tier 1 #6) and system-prompt elision (Tier 1 #4) from this batch; ship as follow-ups after the rest.
- **Pros:** Smaller, faster batch.
- **Cons:** The user's directive: "we are going to solve them all." Splitting the batch invites the same partial-shipping pattern this doc exists to fix.
- **Why not chosen:** Out of step with the stated goal.

### Alternative 3: Big-bang single commit
- **Description:** All seven items in one commit on `v5`.
- **Pros:** Atomic.
- **Cons:** Unreviewable. Each phase here is a self-contained unit with passing CI; bundling them defeats `/how-to-execute-a-plan`'s phase-by-phase verification.
- **Why not chosen:** Wrong granularity for the executor.

### Alternative 4: `SummaryFanout` shape for Plan summaries (a/b/c)

The Plan summary renderer needs both the Plan and its child Works (`summary::write_plan(target, plan, &children)`). A pure decorator over `PlanUpdateSink` cannot read children. Three options were considered before settling on (c):

- **(a) `SummaryFanout` holds `Arc<Store>`.** The decorator owns concrete-store access for the read side, even while it forwards writes through the sink trait.
  - Pros: simplest plumbing; no new traits; no caller-side change.
  - Cons: re-couples the decorator back to the concrete `Store` type, defeating the sink-trait abstraction. Tests need a real or fake `Store` with `WorksStore`.
  - **Why not chosen:** the decorator gaining a concrete-store dependency to satisfy one of three sink impls is the kind of hidden coupling that makes the decorator non-portable.

- **(b) Introduce a `PlanChildrenReader` trait, fan-out takes inner sink + reader.** `SummaryFanout<S, R>` where `S: PlanUpdateSink, R: PlanChildrenReader`.
  - Pros: stays trait-pure; tests inject `MockReader` returning canned children.
  - Cons: more plumbing (one more generic, one more trait); fanout signatures get noisier.
  - **Why not chosen:** over-engineered for this scope; the new trait exists only to satisfy the decorator's read need. Would re-evaluate if Plan's read needs grow beyond `list_by_parent_id`.

- **(c) Change `PlanUpdateSink::update` to take `(plan, children)`.** The caller fetches children before calling `update`; the decorator forwards both args into the renderer.
  - Pros: cleanest decorator for the Plan-update path (no new generic, no concrete-store coupling at the trait); the call site already knows the plan id and is the right place to read.
  - Cons: every Plan-update call site must fetch children, even if it doesn't care about the summary. **Plain c leaves on-disk Plan summaries stale until the Plan itself transitions** — a Plan whose children walk `Pending → InProgress → InReview → Done` shows status counts that lag the actual child state, since each child-Work transition fires only `WorkUpdateSink`.
  - **Why not selected on its own:** the freshness gap defeats the per-transition fanout's reason for existing. Real users will read Plan summary files between Plan transitions and expect them current.

- **(c-extended) Plain c PLUS `WorkUpdateSink` re-renders the parent Plan summary.** [SELECTED] Same `PlanUpdateSink::update(plan, children)` shape as plain c, but the `WorkUpdateSink` impl on `SummaryFanout` also fetches the Work's parent Plan + its children and re-renders the Plan summary on every Work transition. Requires `SummaryFanout` to hold an `Arc<Store>` for the Work-update path's reads; the decorator's purity at the Plan-update trait boundary is preserved (the `PlanUpdateSink` impl is still the clean `(plan, children) → write_plan` forward), and the concrete-store coupling is scoped to the Work-update side where it is genuinely needed.
  - Pros: on-disk Plan summary always reflects the current child state; symmetry-breaking is justified by the actual data dependency rather than an arbitrary choice; the trait-purity goal of plain c is preserved at the Plan-update seam.
  - Cons: one extra `plans().get` and `works().list_by_parent_id` per Work transition; a Work whose parent is not a Plan (e.g. parent is a Spec/Phase, or parent has been deleted) hits a degenerate path that emits `debug!` and skips silently. The c-extended store handle in the decorator is conceptually a regression toward option (a)'s coupling, but scoped to one of three sink impls rather than all of them.
  - **Why chosen:** plain c's freshness gap is the dominant concern. The extra reads per Work transition are cheap and bounded; the degenerate-path handling is small. Picked after architectural review (2026-04-25) flagged the staleness as an unacknowledged tradeoff in plain c.

## Technical Considerations

### Dependencies

No new external crates. Internal additions:

- `crates/store/`: `WorkUpdateSink`, `WorkUpdateError`, `PlanUpdateSink`, `PlanUpdateError`.
- `crates/loopr/src/daemon/summary_fanout.rs` (new module).
- `crates/telemetry/src/digest/{mod,process,session,cost}.rs` (new submodules).
- `crates/llm/`: `Usage` extension, `MeteredLlmClient<L>` wrapper.

### Performance

- Per-call overhead from `SummaryFanout`: one extra atomic file write per FSM transition. Sub-millisecond.
- Per-call overhead from `MeteredLlmClient`: a `Mutex<ProcessSnapshot>` increment after each LLM call. Microseconds.
- System-prompt caching: ~70% reduction in cumulative system-prompt cost across a 5-iteration Implementer Ralph loop. Latency reduction varies (10-30% on the system-prompt portion of pre-fill).
- Session digest aggregation walks `O(processes-in-session)` files; sub-second for first-gate workloads.

### Security

- Digests under XDG (`~/.local/share/loopr/`); inherit user umask. No tokens or per-record content land in the digest, only counters.
- Transcripts continue to land under `<target>/.loopr/records/`, already excluded by `worktree::ensure_loopr_excludes`.
- `cache_control` is server-side metadata; system prompt content goes over the wire identically to today.
- No new external network surface. No new bind ports.

### Testing Strategy

Per phase, listed inline. Cross-cutting invariants verified by the Phase 11 e2e gate.

### Rollout Plan

Single PR on the `v5` branch with eleven commits matching the eleven phases. After merge: `bump -m` (minor) since the batch is feature-bearing.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `taskstore_async::Error::Serde` carries a stringified inner instead of the live `serde_json::Error` | Low | Med | Phase 2 verifies on first edit; if stringified, fall back to substring matching on the error message (`"expected ident"`, `"unexpected end of file"`). |
| Threading `SummaryFanout` through every transition site is mechanical but error-prone | Med | Low | Phase 6 is the substantive refactor; CI catches missed sites at compile time (the type system is the guardrail). |
| `std::panic::set_hook` recurses on its own panic | Low | Med | `AtomicBool` reentrancy guard; second entry skips and continues to panic propagation. |
| Implementer system prompt sits below Sonnet's 2048-token cache minimum | Med | Low | Phase 4 measures and records sizes but proceeds regardless. Below-threshold roles no-op the cache silently; the trait widening + wiring still ships. Win materializes if/when prompt grows or Anthropic lowers thresholds. |
| `LlmClient` trait widening misses a call site or test fake | Low | Med | `cargo check --workspace` is the compile-time guardrail; the trait change is binary (every site needs `let (x, _usage) = ...`). Pre-Phase 4: `grep -rn "complete_free\|complete_with_tool\|impl LlmClient" crates/` produces the full call-site list; verified before edits start. |
| Daemon-boot corruption gate produces false positives that block legitimate boots | Low | High | Phase 2's `--accept-corruption` flag is the operator's escape hatch. Tests cover both gate-trips-and-refuses and gate-trips-but-override-passes paths. |
| Plan-update callers (option (c)) miss the children fetch and pass an empty `Vec<Work>` | Low | Med | The call site list is small (one site today, in the integrator's plan-completion path). The compile-time signature change forces every site to provide a `Vec<Work>`; passing `vec![]` is technically legal but produces an empty Plan summary that the e2e gate catches. |
| Cost rate table in `digest/cost.rs` drifts behind Anthropic's published rates | Med | Low | Single file; updates are one-line. Footnote in digest body: `(rates as of <date>)`. |
| E2E gate flakes due to LLM nondeterminism (decomposition produces different child counts) | Med | Low | E2E asserts on shape (transcript exists, summaries exist, digest exists), not exact child count. The non-determinism is acknowledged. |
| The PR is large enough that a reviewer asks to split it | Med | Low | Each commit is independently reviewable and tests pass at every commit. Splitting after the fact is `git rebase --interactive` with one commit per PR; no rework. |

## Open Questions

All execution-blocking decisions locked. Items below are explicitly deferred with the deferral decision recorded so future readers don't relitigate:

- [ ] **`loopr sessions list` digest indicator.** Show which sessions have a digest vs which don't. One-line change in `list()`. Out of scope here; tracked for a future follow-up.

- [ ] **Auto-corruption-recovery — DEFERRED, no action planned.** Decision: operator-only recovery via `git restore .loopr/taskstore/` or hand-edit. JSONL is git-tracked; the operator already has the right tool. Auto-writing to JSONL based on the daemon's interpretation of corruption is the same failure mode that produces corruption (partial writes). Re-evaluate only after real corruption events in the wild reveal a pattern that actually wants automation. A `--rebuild-cache` flag (wipe SQLite, re-derive from JSONL) is a trivial small follow-up if SQLite-vs-JSONL drift ever bites; not part of this batch.

- [ ] **Wider `cache_control` placement — DEFERRED, revisit after Phase 4 telemetry.** Decision: ship system-prompt-only caching now (Phase 4). Reconsider adding a `tools`-array breakpoint only if cache-hit ratios in process digests show meaningful tokens being left unrecovered. Note: caching `tools` independently requires moving tool schemas out of the system prompt and into the API's separate `tools` field — that's a context-assembly refactor, not a cache tweak. The decision to pursue it should be motivated by actual telemetry data, not symmetry.

- [ ] **Per-Plan / Per-Work OCC — DEFERRED, no action planned.** Decision: keep status quo. Bundle OCC stays; Work and Plan stay non-OCC. The sink-shape asymmetry (Phase 5) is accepted — `BundleUpdateSink` carries `expected_updated_at`, the others don't. Reason: Bundle OCC exists because Reviewer + Integrator have a documented racing pattern; Work and Plan don't have an equivalent documented racer. The daemon's task-spawn serialization plus FSM `Unchanged` detection covers most race shapes already. Re-evaluate when a `warn!` shows up in production logs that an OCC pattern would specifically solve.

Explicitly NOT pursued: `loopr init --dry-run`.

## References

### Inventory and motivating doc
- `docs/three-tiers-of-broken-implementation.md` (the inventory this doc closes)

### Per-item code references

**1. loopr init**
- `crates/loopr/src/commands/init.rs` (current implementation)
- `crates/loopr/src/cli.rs:47-59` (current `Command::Init` doc comment)
- `crates/loopr/CLAUDE.md` (the `loopr init:` bullet)
- `crates/loopr/src/daemon.rs:413` (existing `ensure_loopr_excludes` daemon-side fallback)
- `crates/store/src/store.rs:18` (`TASKSTORE_SUBPATH`)
- `crates/worktree/src/excludes.rs:36` (`ensure_loopr_excludes`)

**2. LooprError variants**
- `crates/loopr/src/error.rs` (variants being removed)
- `crates/loopr/src/lib.rs:181-184, 198` (the two callers)
- `crates/loopr/src/tests.rs:84-98` (pinning tests)
- `crates/loopr/src/transport/client.rs:94`, `crates/loopr/src/cli.rs:41` (doc comments)

**3. Decomposer transcripts**
- `crates/decomposer/src/decompose.rs` (function being wired)
- `crates/telemetry/src/transcript/mod.rs` (`append_iteration` API)
- `crates/agents/src/implementer.rs:43-65` (`write_implementer_transcript` template)

**4. System-prompt elision**
- `crates/llm/src/anthropic.rs` (request builder)
- `crates/llm/src/client.rs` (`LlmClient` trait, unchanged)
- `crates/agents/src/implementer.rs:194` (Ralph-loop call site)
- Anthropic prompt caching docs: https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching

**5. Per-record summaries**
- `crates/loopr/src/summary/` (existing renderers + writers)
- `crates/loopr/src/daemon/context.rs:795-820, 661-664` (existing best-effort wrappers + their only call site)
- `crates/store/src/bundles.rs:170-235` (`BundleUpdateSink` shape to mirror)
- `crates/integrator/src/lib.rs:123, 478, 503` (Integrator's existing sink consumption)

**6. Process / session digests**
- `docs/design/2026-04-24-loopr-layout.md` (the layout that reserves digest paths)
- `crates/loopr/src/daemon.rs:506` (`serve_core` where graceful-exit hook lands)
- `crates/loopr/src/commands/sessions.rs:67-76` (`sessions end` where session digest writer lands)
- `crates/loopr/src/daemon/context.rs:60-74` (`shutdown_notify` plumbing)

**7. Dead-code allowances + Corruption**
- `crates/loopr/src/daemon/context.rs:749, 773` (stale `#[allow(dead_code)]`)
- `crates/loopr/src/daemon/startup.rs:234` (live caller of `transition_and_persist_bundle`)
- `crates/loopr/src/daemon/context.rs:643` (live caller of `transition_and_persist_plan`)
- `crates/store/src/error.rs` (`Corruption` variant being wired)
- `serde_json::error::Category` documentation

### Process and shape
- `docs/vision.md` "Working rules" (post-rule-#1 removal)
- `docs/vision.md` "Observability" "Four-layer log strategy" (becomes five layers in Phase 10)
- `docs/design/2026-04-24-prompts-on-disk.md` (parent doc that left items 3 + 4 open)
- `docs/design/2026-04-22-stage-8-wiring.md` Phase 8.5 (parent doc that posited items 5 + 7's missing sinks)
