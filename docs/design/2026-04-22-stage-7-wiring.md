# Design Document: Stage 7 Wiring — agents × tools × loopr

**Author:** Claude (with Scott)
**Date:** 2026-04-22
**Status:** Implemented
**Review Passes Completed:** 5/5 + Architect R1

## Summary

Wire together the three Stage-7 docs (tool-registry, worktree-lifecycle, implementer) so a Work actually flows through the pipeline and produces a Bundle. Two code pieces are missing: a production `impl ToolExecutor` bridging `agents`'s trait to `tools::dispatch`, and a daemon-side routine that, after `decomposer` persists Works, creates a worktree and runs `run_implementer` against it. Stage 7's exit criterion ("Bundle whose commit diff shows real file edits") depends on both.

## Problem Statement

### Background

The Stage-7 roadmap row decomposed into three per-crate design docs:

- `crates/tools/docs/design/2026-04-21-tool-registry.md` (Implemented, v0.5.21) — `Tool` trait, 6 builtins, `tools::dispatch`, lane router, bwrap, denylist.
- `docs/design/2026-04-21-worktree-lifecycle.md` (Implemented, v0.5.23) — `Worktree::create`/`cleanup`, reconcile primitives.
- `docs/design/2026-04-21-implementer.md` (Implemented, v0.5.24) — `run_implementer<L,T,S,C>` ralph loop, `Deps` bundle, `ToolExecutor` + `BundleSink` seams.

Each doc's code landed in its own crate. Each seam is typed and testable. `BundleSink` even has its production impl (`impl BundleSink for store::Store` in `crates/agents/src/implementer.rs:51`).

### Problem

Two seams were specified but no production-side of each was written:

1. **`agents::dispatch::ToolExecutor` has no production impl.** All three impls in the tree (`dispatch/tests.rs`, `implementer/tests.rs`, `tests/seam_implementer.rs`) are test fakes. Nothing converts `(tool_name, input, working_dir)` into a real `tools::ToolContext` + `tools::dispatch(...)` call.
2. **The daemon never calls `run_implementer`.** `crates/loopr/src/transport/handler.rs:107` `handle_plan_create` persists a Plan, runs `decomposer::decompose`, persists Works — and returns. No code path picks up a newly-persisted Work, spins a worktree, and runs the Implementer.

Each per-crate doc was scoped honestly to one crate. The wiring **between** them fell through the cracks because no one design doc owned the cross-crate seam. Stage 7's exit criterion is an end-to-end behavior; the three existing docs collectively describe the components but not the assembly.

### Goals

- `impl ToolExecutor` for a new `agents::RealTools` struct (production path, co-located with `BundleSink`'s production impl for pattern consistency) that calls `tools::dispatch` with a properly-constructed `ToolContext`.
- Add three new fields to `DaemonContext` — `context_builder`, `implementer_config`, `implementer_tasks: JoinSet<()>` — leaving the existing tool infrastructure (`router`, `sandbox`, `bash_denylist`, `path_deny_patterns`, `llm`) untouched.
- Persist implementer-error Work transitions durably via a new `WorksStore::update(&self, work: Work) -> Result<(), StoreError>` — single-line delegation to `AsyncStore::update` (confirmed at `taskstore-async/src/store.rs:122`). Daemon restart sees the latest status on disk; no infinite-retry loop on failing Works.
- Post-decompose dispatch path in `transport/handler.rs`: after `handle_plan_create` persists a Work DAG, the daemon spawns one tokio task per new Work that creates a worktree, assembles `Deps`, runs `run_implementer`, and disposes of the worktree per `AttemptCleanupPolicy`.
- Stage-7 exit criterion met: a real `.loopr/taskstore/bundles.jsonl` entry with a `head_commit` on the worktree branch, containing a real file edit, reproducible on a toy target.

### Non-Goals

- **Reviewer, Integrator, Tick production.** Stage 8. This doc's success condition stops at "Bundle persisted with real commit."
- **A proper Work-state reactor that polls the store.** Stage 7's MVP dispatches directly from the request handler after decompose succeeds. A reactor pattern (daemon subscribes to Work-FSM transitions; any `Pending`/`Ready` Work triggers an Implementer) is architecturally correct per the reactive-daemon model in vision.md but is not required by the Stage-7 exit criterion. Deferred with a documented seam.
- **Parallel Work execution.** Vision line 609 says "one Work at a time until serial proves the shape." Serial dispatch — one active `run_implementer` task at a time per Plan — is the Stage-7 scope.
- **Director escalation.** `ImplementerError::EscalationNeeded` terminates the task and marks the Work failed; no Director is spawned. Vision line 607.
- **Per-attempt retry on top of the Implementer's own retries.** The Implementer's ralph loop handles correction internally (`max_iterations`, Lifeguard). Daemon doesn't wrap it in an outer retry.
- **Process-rule design-doc meta-change.** The observation that Stage 7 needed a fourth capstone doc is called out in the "Process Observation" section but does not propose rule changes in this doc; that belongs in a separate amendment to `CLAUDE.md` or `docs/roadmap.md`.

## Proposed Solution

### Overview

Two concrete additions:

1. **`agents::RealTools`** — a small struct in `crates/agents/src/dispatch.rs` (same file as the `ToolExecutor` trait; mirrors the `BundleSink` production-impl pattern) that owns the cheap-to-clone `ToolContext` prerequisites and implements `ToolExecutor` by building a per-call `ToolContext` and forwarding to `tools::dispatch`. Per-Work instance: each Work's implementer gets its own `RealTools` with that Work's `persist_base`.
2. **`DaemonContext::spawn_implementer_for_work`** — a method on the existing `DaemonContext` that, given a persisted `Work`, constructs a `Worktree`, builds `agents::implementer::Deps`, runs `run_implementer`, transitions the Work on error, and cleans the worktree. Called from `handle_plan_create` after decompose + persist succeeds, once per new Work, each spawned into a `JoinSet` field on `DaemonContext`.

**Reusing existing daemon state.** `DaemonContext` already owns the Stage-7 tool infrastructure: `router: Arc<LaneRouter>`, `sandbox: SandboxMode`, `bash_denylist: Arc<BashDenylist>`, `path_deny_patterns: Vec<String>`, `llm: Arc<AnthropicClient>` (concrete, **not** `Arc<dyn LlmClient>`), and `store: Store`. The existing `DaemonContext::tool_context(work_id, invocation_id) -> ToolContext` helper already shows the construction recipe. It is NOT reused directly by `RealTools` because `tool_context` hard-codes `working_dir = self.target`, while `RealTools::execute` needs `working_dir = worktree_path`; the helper's body is the reference implementation, and `RealTools::execute` mirrors it with the working-dir substitution.

No changes to the existing `ToolExecutor` trait signature, no changes to `run_implementer`, no changes to `tools::dispatch`. All three Stage-7 docs stay implemented as shipped.

### Architecture

```
┌───────────────────────────────────────────────────────────────────────┐
│ loopr daemon                                                          │
│  transport/handler.rs: handle_plan_create                             │
│    ├── persist Plan                                                   │
│    ├── decomposer::decompose → Vec<Work>                              │
│    ├── persist Works                                                  │
│    └── for work in works:                                             │
│         ctx.implementer_tasks.lock().await.spawn(                     │
│           Arc::clone(ctx).spawn_implementer_for_work(work))           │
│              │                                                        │
│              ▼                                                        │
│  daemon/context.rs::spawn_implementer_for_work(self: Arc<Self>, Work) │
│    ├── base_sha = tokio git rev-parse HEAD (async, in self.target)    │
│    ├── spawn_blocking { Worktree::create(target, root, id, base) }    │
│    │      (sync Worktree crate, isolated off the tokio reactor)       │
│    ├── tool_schemas = tools::all_schemas()                            │
│    ├── Deps { llm: &*self.llm,          // &AnthropicClient: LlmClient │
│    │          tools: RealTools::new(self.router.clone(), ...),        │
│    │          bundles: &self.store,     // &Store: BundleSink          │
│    │          context: Arc::clone(&self.context_builder),             │
│    │          config: self.implementer_config.clone(),                │
│    │          tool_schemas,                                           │
│    │          state: StateSummary::default() }                        │
│    ├── run_implementer(&work, &worktree, &deps).await                 │
│    │   → Ok(bundle) | Err(ImplementerError)                           │
│    ├── on Ok: bundle already persisted by run_implementer             │
│    │        ; Work state untouched (implementer owns happy path)      │
│    ├── on Err(EscalationNeeded):                                      │
│    │     work.status = Blocked; self.store.works().update(work).await │
│    ├── on Err(other):                                                 │
│    │     work.status = Failed + FailureReason::Other(e.to_string());  │
│    │     self.store.works().update(work).await                        │
│    └── match policy {                                                 │
│          Immediate | OnWorkTerminal =>                                │
│            spawn_blocking { worktree.cleanup() },                     │
│          OnRunEnd => defer to daemon_main shutdown path,              │
│          Never => leave worktree in place (debug only)                │
│        }  // worktree BRANCH always kept per vision:135 (Stage 8)     │
└──────────────────────────────────────────────┬────────────────────────┘
                                               │
                                               ▼
┌───────────────────────────────────────────────────────────────────────┐
│ agents::RealTools (new) — in crates/agents/src/dispatch.rs            │
│                                                                       │
│  struct RealTools {                                                   │
│    router: Arc<tools::LaneRouter>,                                    │
│    sandbox: tools::SandboxMode,                                       │
│    bash_denylist: Arc<tools::BashDenylist>,                           │
│    path_deny_patterns: Vec<String>,                                   │
│    persist_base: Option<PathBuf>,   // per-run dir                    │
│  }                                                                    │
│                                                                       │
│  impl ToolExecutor for RealTools {                                    │
│    async fn execute(name, input, working_dir) -> Result<String, _> {  │
│      let ctx = tools::ToolContext {                                   │
│        working_dir: working_dir.to_path_buf(),                        │
│        router: self.router.clone(),                                   │
│        sandbox: self.sandbox,                                         │
│        path_deny_patterns: self.path_deny_patterns.clone(),           │
│        bash_denylist: self.bash_denylist.clone(),                     │
│        persist_base: self.persist_base.clone(),                       │
│        invocation_id: Some(Uuid::now_v7()),                           │
│      };                                                               │
│      let value = tools::dispatch(name, input.clone(), &ctx).await     │
│                     .map_err(|e| DispatchError::Tool(e.to_string()))?;│
│      Ok(serde_json::to_string_pretty(&value)                          │
│           .unwrap_or_else(|_| value.to_string()))                     │
│    }                                                                  │
│  }                                                                    │
└───────────────────────────────────────────────────────────────────────┘
```

### Data Model

**New struct in `crates/agents/src/dispatch.rs`:**

```rust
/// Production implementation of `ToolExecutor`. Thin adapter: builds a
/// `tools::ToolContext` per invocation and forwards to `tools::dispatch`.
/// All shared state (router, sandbox posture, denylist) is Arc'd in from
/// the owning `DaemonContext`.
pub struct RealTools {
    router: Arc<tools::LaneRouter>,
    sandbox: tools::SandboxMode,
    bash_denylist: Arc<tools::BashDenylist>,
    path_deny_patterns: Vec<String>,
    persist_base: Option<PathBuf>,
}

impl RealTools {
    pub fn new(
        router: Arc<tools::LaneRouter>,
        sandbox: tools::SandboxMode,
        bash_denylist: Arc<tools::BashDenylist>,
        path_deny_patterns: Vec<String>,
        persist_base: Option<PathBuf>,
    ) -> Self { ... }
}

impl ToolExecutor for RealTools {
    fn execute<'a>(
        &'a self,
        tool_name: &'a str,
        input: &'a serde_json::Value,
        working_dir: &'a Path,
    ) -> impl Future<Output = Result<String, DispatchError>> + Send + 'a {
        async move {
            let ctx = tools::ToolContext {
                working_dir: working_dir.to_path_buf(),
                router: self.router.clone(),
                sandbox: self.sandbox,
                path_deny_patterns: self.path_deny_patterns.clone(),
                bash_denylist: self.bash_denylist.clone(),
                persist_base: self.persist_base.clone(),
                invocation_id: Some(uuid::Uuid::now_v7()),
            };
            let value = tools::dispatch(tool_name, input.clone(), &ctx)
                .await
                .map_err(|e| DispatchError::Tool(e.to_string()))?;
            Ok(serde_json::to_string_pretty(&value)
                .unwrap_or_else(|_| value.to_string()))
        }
    }
}
```

**New fields on `crates/loopr/src/daemon/context.rs::DaemonContext`** (existing fields unchanged — per the current `DaemonContext` struct, which already holds `target`, `run_id`, `store`, `llm: Arc<AnthropicClient>`, `router: Arc<LaneRouter>`, `bash_denylist: Arc<BashDenylist>`, `path_deny_patterns`, `sandbox`, the shutdown machinery, and a `tool_context()` helper):

```rust
pub struct DaemonContext {
    // ... all existing fields (target, run_id, store, llm, router,
    //     bash_denylist, path_deny_patterns, sandbox, events,
    //     shutting_down, shutdown_notify, started_at, pid) ...

    // new for Stage 7 wiring:
    pub context_builder: Arc<context::InlineContextBuilder>,
    pub implementer_config: agents::ImplementerConfig,
    pub implementer_tasks: tokio::sync::Mutex<tokio::task::JoinSet<()>>,
}
```

Daemon startup (`daemon::run_active_daemon`) constructs `InlineContextBuilder` once, reads `ImplementerConfig` from the composed top-level config, and initializes the `JoinSet`. `DaemonContext::new`'s signature grows by these three parameters.

**Handler dispatch takes `Arc<DaemonContext>`.** Current transport shape is `pub async fn dispatch(req, state, ctx: &DaemonContext)`. Spawning a task that outlives the handler call requires an `Arc` to clone from. Change to `ctx: &Arc<DaemonContext>` so handlers can `Arc::clone(ctx)` before spawning. All existing borrow sites (`ctx.target`, `ctx.store.plans()`, etc.) keep working because `&Arc<T>` auto-derefs to `&T`. One-line edit to each handler signature; no call-site changes needed at the auto-deref points.

**Store shutdown contract extends.** The current comment on `DaemonContext::store` (context.rs:55–67) requires every `Arc<DaemonContext>` clone to drop before `daemon_main`'s shutdown path calls `Arc::try_unwrap` to reclaim the `Store` for `.close().await`. Spawned implementer tasks hold an `Arc<DaemonContext>` clone, so daemon shutdown MUST drain `implementer_tasks.lock().await` via `join_next()` with a soft timeout before returning from `daemon_main`, in addition to the existing accept-loop / signal-watcher drains. Documented in Phase 4.

**No new records, no FSM changes.** Work-state transitions on implementer success/failure use the existing `WorkStatus` FSM variants; Implementer leaves Work in whatever state, daemon only transitions on the error paths.

### API Design

**New public items in `agents`:**

```rust
// crates/agents/src/dispatch.rs
pub struct RealTools { /* ... */ }
impl RealTools { pub fn new(...) -> Self }
impl ToolExecutor for RealTools { /* ... */ }
```

Re-exported from `crates/agents/src/lib.rs`:

```rust
pub use dispatch::{ActionResult, DispatchError, RealTools, ToolExecutor, dispatch_action};
```

**New internal items in `loopr`:**

```rust
// crates/loopr/src/daemon/context.rs
impl DaemonContext {
    pub async fn spawn_implementer_for_work(self: Arc<Self>, work: domain::Work) -> ();
}
```

The method takes `Arc<Self>` so it can be called from inside a `tokio::spawn` without borrowing. The return type is `()` because errors are logged + reflected into Work state; nothing upstream needs to await the Bundle for Stage 7 (Stage 8 wires Reviewer to Bundle events).

**New call site in `transport/handler.rs`:**

```rust
// after decompose + persist Works
for work in persisted_works {
    let task_ctx = Arc::clone(ctx);
    ctx.implementer_tasks
        .lock()
        .await
        .spawn(task_ctx.spawn_implementer_for_work(work));
}
```

### Implementation Plan

#### Phase 1: `RealTools` adapter + unit tests
**Model:** sonnet

- Add `RealTools` struct and `impl ToolExecutor for RealTools` in `crates/agents/src/dispatch.rs`.
- Unit test in `crates/agents/src/dispatch/tests.rs`: construct a `RealTools` with a minimal `LaneRouter` + empty `BashDenylist`, dispatch a `read` on a tempdir fixture, assert the returned `String` parses back as JSON with the file contents.
- Re-export from `agents::lib`.
- No changes to the trait, no changes to existing impls.

#### Phase 2: `DaemonContext` grows three new fields + `WorksStore::update`
**Model:** sonnet

- Add `context_builder: Arc<context::InlineContextBuilder>`, `implementer_config: agents::ImplementerConfig`, `worktree_cleanup_policy: AttemptCleanupPolicy`, `implementer_tasks: Mutex<JoinSet<()>>` to `DaemonContext`.
- Extend `DaemonContext::new` signature by the four new parameters; update `daemon::run_active_daemon` to construct them and pass through. Router / sandbox / denylist construction remains untouched — already done in the existing code path. `worktree_cleanup_policy` comes from `.loopr/config.yml` with default `OnWorkTerminal` per vision amendment a5.
- Update `transport::handler::dispatch` and the four `handle_*` fn signatures from `ctx: &DaemonContext` to `ctx: &Arc<DaemonContext>`. Auto-deref keeps existing borrow sites compiling.
- Add `WorksStore::update(&self, work: Work) -> Result<(), StoreError>` in `crates/store/src/works.rs`, parallel to `create`. Body: `self.inner.update(work).await.map_err(Into::into)?; Ok(())`. Mirror the same pattern for `PlansStore::update` if not already present (check at implementation time). Unit test: create a Work, mutate status, call update, re-read and assert status round-tripped.
- Unit test: `DaemonContext::new` accepts the four new parameters and exposes them on the fields.

#### Phase 3: `spawn_implementer_for_work` + error-path Work transitions
**Model:** opus

- Implement `DaemonContext::spawn_implementer_for_work(self: Arc<Self>, work: domain::Work) -> ()`:
  - `base_sha`: shell out to `git rev-parse HEAD` in `self.target` via `tokio::process::Command::output().await` (mirrors `agents::dispatch::rev_parse_head` at `dispatch.rs:190`, which IS async — earlier "synchronous" wording was wrong; `tokio::process` keeps git subprocess spawning off the sync path). Helper lives inline in `daemon/context.rs` or a small `daemon::git` module; not exposed cross-crate.
  - `worktree_root = self.target.join(".loopr/worktrees")`.
  - `persist_base = self.target.join(".loopr/runs").join(self.run_id.as_str()).join("work").join(work.id.as_ref())`; `fs::create_dir_all(&persist_base).ok()`.
  - **Worktree ops run in `spawn_blocking`.** `Worktree::create` and `Worktree::cleanup` are synchronous (`crates/worktree/src/handle.rs:62`, `:81`) and internally spawn `std::process::Command` with blocking waits. Per `docs/vision.md:134`, invoke them inside `tokio::task::spawn_blocking(...)` so the tokio reactor isn't blocked:
    ```rust
    let target = self.target.clone();
    let root = worktree_root.clone();
    let wid = work.id.clone();
    let base = base_sha.clone();
    let worktree = tokio::task::spawn_blocking(
        move || Worktree::create(&target, &root, &wid, &base)
    ).await??;
    ```
    (`.await??` unwraps the `JoinError` then the `WorktreeError`.) Same pattern for `cleanup()`. The lane-router/Local-Net-Heavy model does NOT apply here — lanes classify Tool calls, not daemon-internal git plumbing.
  - `tools = RealTools::new(self.router.clone(), self.sandbox, self.bash_denylist.clone(), self.path_deny_patterns.clone(), Some(persist_base))`.
  - `tool_schemas = tools::all_schemas()` (snapshot; vision D3 from implementer.rs).
  - `deps = Deps { llm: &*self.llm, tools, bundles: &self.store, context: Arc::clone(&self.context_builder), config: self.implementer_config.clone(), tool_schemas, state: StateSummary::default() }`.
    - `llm: &*self.llm` follows the same Arc-deref-to-borrow pattern the daemon already uses at the `decomposer::decompose(&plan, &ctx.target, &*ctx.llm)` call site; `impl LlmClient for &AnthropicClient` exists in the tree.
    - `bundles: &self.store` works because `impl BundleSink for store::Store` is already defined (`implementer.rs:51`), so `&Store` satisfies `BundleSink` via auto-ref.
  - `result = run_implementer(&work, &worktree, &deps).await`.
  - Match:
    - `Ok(_bundle)`: already persisted by `run_implementer` (via `BundleSink::persist` at `implementer.rs:175/180/210/215/332`). Log `info!(bundle_id, "implementer produced bundle")`. Work state untouched on the happy path.
    - `Err(ImplementerError::EscalationNeeded(reason))`: `work.status = WorkStatus::Blocked`; `self.store.works().update(work).await.ok()` (log + swallow on persistence failure so the task completes); `warn!(%reason, "implementer escalated")`.
    - `Err(other)`: `work.status = WorkStatus::Failed`; set `work.failure_reason = Some(FailureReason::Other(other.to_string()))` (verify exact field name in `domain::Work` at implementation time); `self.store.works().update(work).await.ok()`; `error!(%other, "implementer error")`.
  - **Crucially**, without `WorksStore::update` (Phase 2), error transitions only live in memory and are lost on daemon restart, producing an infinite-retry loop where a failing Work is re-dispatched as Pending forever. `WorksStore::update` is a Phase-2 prerequisite, not optional.
  - Worktree cleanup with explicit policy match:
    ```rust
    match self.worktree_cleanup_policy {
        AttemptCleanupPolicy::Immediate | AttemptCleanupPolicy::OnWorkTerminal => {
            tokio::task::spawn_blocking(move || worktree.cleanup())
                .await.ok();  // cleanup is best-effort
        }
        AttemptCleanupPolicy::OnRunEnd => {
            // leave worktree; daemon_main shutdown path sweeps
        }
        AttemptCleanupPolicy::Never => {
            warn!("AttemptCleanupPolicy::Never — leaking worktree (debug only)");
        }
    }
    ```
    The worktree **branch** is always kept regardless of policy (vision:135 — Stage 8 Integrator needs it).
- `#[tracing::instrument(level = "info", skip_all, fields(work_id = %work.id, run_id = %self.run_id))]` on the method.
- Unit test: stubbed `LlmClient` returning a scripted `write` + `propose_bundle` sequence, real tempdir target with `git init`, assert a Bundle row appears in `self.store.bundles().list()` and the worktree branch has the expected commit.

#### Phase 4: Handler call-site + JoinSet drain at shutdown
**Model:** sonnet

- In `handle_plan_create`, after the existing `decomposer::decompose` + Works-persist block succeeds, iterate the persisted Works and for each: `ctx.implementer_tasks.lock().await.spawn(Arc::clone(ctx).spawn_implementer_for_work(work));`.
- In `daemon_main`'s shutdown path (the current sequence: accept-loop exits → handler JoinSet drains → signal-watcher task joins → `Arc::try_unwrap` on `DaemonContext` → `Store::close().await`), insert a new drain step for `implementer_tasks` AFTER handlers drain and BEFORE the signal-watcher join. Use `join_next().await` in a loop with `tokio::time::timeout`; on timeout, call `abort_all()` and log the work-ids that were aborted (reachable via `work_id` span field on each future's instrumentation).
- Document in `context.rs`'s Store-ownership contract comment that `implementer_tasks` is a third holder of `Arc<DaemonContext>` clones that must drain before `try_unwrap`.
- Integration test in `crates/loopr/tests/stage_7_handle_plan_create.rs`: target tempdir with `git init`, mock LLM feeding a scripted write+propose, assert `.loopr/taskstore/bundles.jsonl` has exactly one line after the test shuts the daemon down.

#### Phase 5: Stage-7 exit-criterion smoke test
**Model:** opus

- `crates/loopr/tests/stage_7_e2e.rs`: full daemon boot in a subprocess against a scaffolded toy target. Stub or real LLM (feature-flagged) produces one file edit. Assert the target's HEAD on the worktree branch has the expected commit diff.
- Roadmap update: flip Stage 7's status to "complete" (convention proposed in "Process Observation"), update the per-doc pointers to include this doc, and record the shipped version.

## Alternatives Considered

### Alternative 1: Put the bridge in `loopr`, not `agents`

- **Description:** Define `RealTools` in `crates/loopr/src/daemon/tools.rs` since `loopr` is the assembly layer.
- **Pros:** `agents` stays purely trait-defining for tool-executor; `loopr` owns all concrete glue.
- **Cons:** Breaks symmetry with `BundleSink`, whose production impl lives in `agents` next to its trait (`crates/agents/src/implementer.rs:51`). Requires `loopr` to depend on `tools`-internal types (`LaneRouter`, `BashDenylist`, `ToolContext` fields) that are already Arc-cloneable; no gain over putting it in `agents` where `tools` is already a dependency.
- **Why not chosen:** Symmetry with `BundleSink` wins. Both are "the production impl of an `agents` trait over an external crate's type"; both belong in `agents::dispatch` next to the trait. `loopr` constructs the Arc'd inputs; `agents` holds the shape.

### Alternative 2: Make `run_implementer` take concrete types instead of the `ToolExecutor` trait

- **Description:** Drop the `ToolExecutor` trait; make `run_implementer` take `&tools::LaneRouter` + a ToolContext-factory directly.
- **Pros:** One fewer seam; no need for a bridge type.
- **Cons:** Violates the `Deps<L,T,S,C>` DI pattern documented in `crates/agents/CLAUDE.md`. Removes test-fake seam the Implementer already uses in `dispatch/tests.rs` + `implementer/tests.rs`. Requires churning the Implementer doc (Implemented, v0.5.24) instead of adding an adapter.
- **Why not chosen:** The trait-based seam is already the shipped design; the bridge is cheaper than re-shaping it.

### Alternative 3: A daemon-internal Work-state reactor that polls TaskStore

- **Description:** Instead of spawning an Implementer inline after `handle_plan_create`, have a background task that polls `.loopr/taskstore/works.jsonl` for `Pending` works and dispatches each.
- **Pros:** Closer to the "reactive daemon" model in vision.md. Handles restart/reconcile for free (crashed Works picked up on next poll cycle).
- **Cons:** Heavier — needs a poll interval, a dedupe mechanism, a notification channel for fast reaction. More surface to test. Not required by Stage 7's exit criterion.
- **Why not chosen:** Right thing for Stage 7.5+. Stage-7 goal is a single E2E pass, not production-grade reactivity. Documented as a follow-up in the Open Questions.

### Alternative 4: Write this as an amendment to `2026-04-21-implementer.md` rather than a new doc

- **Description:** Extend the implementer doc with a "Production Integration" section covering `RealTools` and the daemon wiring.
- **Pros:** One doc to read for the full implementer story.
- **Cons:** Cross-crate content in a crate-scoped doc violates `docs/CLAUDE.md`'s placement rule. Reopens an Implemented-marked doc.
- **Why not chosen:** Placement rule is load-bearing. Top-level integration doc is the right home.

## Technical Considerations

### Dependencies

No new external crates. All types already in the tree:

- `uuid` (already workspace) — for `Uuid::now_v7()` per invocation.
- `serde_json` (already workspace) — for `to_string_pretty`.
- `tokio::task::JoinSet` (already a tokio feature) — for daemon-owned task tracking.
- `tokio::sync::Mutex` (already available) — wrapping `JoinSet`.

No new `Cargo.toml` dependencies in any crate.

### Performance

- `RealTools::execute` allocates one `ToolContext` per call (5 Arc clones + 2 Vec clones). Per-call overhead is microseconds; dominated by the subprocess/filesystem work the tool itself performs.
- `JoinSet` per-Work spawn overhead: one `tokio::spawn` per Work (Stage 7: typically 1-3 Works per Plan). Negligible.
- `serde_json::to_string_pretty` allocates proportional to tool output size. Output is already truncated to 32K at the tools layer (MAX_INLINE_OUTPUT, per tool-registry D3), so the string is bounded.

### Security

- `path_deny_patterns` flows unchanged from daemon config through `ToolContext`; `RealTools` does not weaken it.
- `sandbox` mode is read once at startup and propagated immutably. Agent cannot request a weaker sandbox per-invocation.
- `BashDenylist` is shared `Arc`, not cloned per-call; guarantees a single daemon-wide denylist view.
- `persist_base` is fixed per run-id at daemon startup. Agent cannot redirect persist output to another location.
- Worktree branch retained after cleanup (vision line 135) — agent output survives beyond the Implementer for Integrator; no new leak path.

### Testing Strategy

Per `CLAUDE.md` "Seam tests, not only unit tests":

**Unit:**
- `RealTools::execute` with a tempdir + `read` tool: returns JSON string parseable back to a map with `contents`.
- `RealTools::execute` with a denied path pattern: returns `Err(DispatchError::Tool(...))`.

**Seam (agents × tools):**
- `RealTools` satisfies the same existing seam tests that `FakeTools` does (`tests/seam_implementer.rs`) when pointed at a real `LaneRouter` + tempdir.

**Integration (loopr):**
- Daemon boot → `handle_plan_create` → one Work scheduled → stubbed LLM returns a write+propose → Bundle lands in the store. Under `crates/loopr/tests/`.
- Stage-7 smoke test: full daemon on a scaffolded toy target (`tests/stage_7_e2e.rs`). Asserts a worktree-branch commit exists with the expected diff.

**Out of scope for this doc:**
- Tests against the real Anthropic API. Stage 9 first-gate covers live-API E2E.
- Concurrent Work dispatch. Stage 7 is serial per vision.md line 609.

### Rollout Plan

Single branch (`v5`), single tag bump. Target version: next available (`v0.5.25` or successor). No feature flag; no migration. Pre-release verification: `otto ci` at repo root (full workspace) + manual smoke test on a scaffolded `rust-version`-style target.

## Acceptance Criteria

All must be true before Stage 7 is declared complete:

- `agents::RealTools` exists, implements `ToolExecutor`, is re-exported from `crates/agents/src/lib.rs`
- `WorksStore::update` exists in `crates/store/src/works.rs` with a passing round-trip test
- `DaemonContext` carries `context_builder`, `implementer_config`, `worktree_cleanup_policy`, `implementer_tasks` fields
- `transport/handler::handle_plan_create` spawns one task per persisted Work into `ctx.implementer_tasks` after decompose
- `daemon_main` shutdown path drains `implementer_tasks` (with soft timeout + `abort_all()` fallback) before `Arc::try_unwrap` on `DaemonContext`
- Every `Worktree::create` and `worktree.cleanup()` call site runs inside `tokio::task::spawn_blocking`
- On `Err(EscalationNeeded)` the Work's status on disk reads `Blocked` after daemon restart (not `Pending`)
- On other `Err(_)` the Work's status on disk reads `Failed` with `FailureReason::Other(...)` after daemon restart
- `cargo test -p agents`, `-p store`, `-p loopr` all pass; `otto ci` at repo root passes
- Integration test in `crates/loopr/tests/stage_7_handle_plan_create.rs` passes: stubbed LLM → one `write`+`propose_bundle` → exactly one line in `.loopr/taskstore/bundles.jsonl`
- Smoke test on a toy target: `loopr plan "add foo"` produces a `.loopr/taskstore/bundles.jsonl` entry whose `head_commit` points at a commit on the worktree branch whose diff contains a real file edit

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| `run_implementer` error variants not exhaustively mapped to Work-FSM transitions | Med | Med | Phase 3 implementer matches all `ImplementerError` variants; `#[non_exhaustive]` not set, so compiler catches additions |
| **In-memory-only Work transitions lost on daemon restart → infinite-retry loop on failing Works** (Architect R1 finding) | **High if unmitigated** | High | `WorksStore::update` added in Phase 2 (backed by `AsyncStore::update` at `taskstore-async/src/store.rs:122`). Phase 3 calls `self.store.works().update(work).await` after every error-path status transition. Without this, a failing Work stays Pending on disk and the daemon re-dispatches it every boot |
| **Synchronous `Worktree::create` / `cleanup` blocking the tokio reactor** (Architect R1 finding) | Med | High | Every `Worktree::create` + `worktree.cleanup()` call site wrapped in `tokio::task::spawn_blocking` per vision.md:134. Lane router does NOT cover these (lanes are for Tool calls only). `.await??` pattern for error unwrapping the `JoinError` + inner `WorktreeError` |
| `AttemptCleanupPolicy::Never` leaks worktrees across many runs | Low | Med | Vision a5 already flags as debug-only; daemon warn-log on startup if Never is configured |
| `JoinSet` grows unbounded if daemon keeps accepting Plans without drain | Low | Med | Per-Plan join_set instead of global; alternately, Stage 7.5 budget enforcement ties in here |
| Serializing full `Value` output via `to_string_pretty` inflates history past the context window | Med | Med | Tools already truncate subprocess output at 32K. Worst case: LLM truncates further in `ContextBuilder`. Document the boundary; add per-call size log |
| Implementer panics (not caught by `catch_unwind`) escape into daemon task | Low | High | Vision line 417 mandates `catch_unwind` at agent-task boundary; verify in Phase 4 integration test that a panicking fake-LLM does not kill the daemon |
| Worktree-registry reconcile at daemon startup (a2/a3 from vision) trips over Works we just spawned | Low | Low | Reconcile runs before the socket binds; first `handle_plan_create` cannot arrive until reconcile completes. Document the ordering guarantee in Phase 2 |
| `uuid::Uuid::now_v7()` clashes inside one nanosecond on a busy implementer | Negligible | Low | v7 embeds a timestamp + random suffix; documented as non-colliding in practice |
| Daemon shutdown doesn't await in-flight Implementer tasks, losing Bundles | Med | High | Phase 4 extends shutdown to drain `join_set` with a soft timeout before returning from `daemon_main` |

## Process Observation

Stage 7 shipped three per-crate design docs, each truthfully marked `Status: Implemented`, yet the stage's end-to-end exit criterion was not met because the cross-crate assembly had no design-doc owner. The recurring failure mode: decomposing a stage into per-crate docs leaves the integration work homeless, because each per-crate doc is legitimately scoped to one crate.

A durable fix belongs in `docs/roadmap.md` or the project-wide `CLAUDE.md`, not this doc. Suggested shape (to be proposed separately):

- Every roadmap stage whose exit criterion spans multiple crates MUST have a top-level "wiring" / "capstone" design doc as part of its doc set, in addition to whatever per-crate docs it spawns.
- Stage rows in `docs/roadmap.md` carry a `Status:` line (not just the introduction) that flips only when the exit criterion is demonstrated against a real run, not when the last constituent doc lands.
- Each design doc, when landed, replaces its roadmap placeholder path with a dated filename + version-shipped annotation, so `roadmap.md` is a live index of what exists rather than a frozen plan.

Raised here because this doc is itself the first instance of the "capstone" pattern it proposes. If the pattern is accepted, future Stages 8 and 9 should follow suit.

## Open Questions

- [ ] **`BundleSink for &T` forwarding impl.** `Deps<S: BundleSink>` holds `bundles: S` by value; Phase 3 wants `bundles: &self.store`, which needs `&Store: BundleSink`. Add `impl<T: BundleSink + ?Sized> BundleSink for &T` in `crates/agents/src/implementer.rs` as a non-breaking blanket forwarding impl. Verify the async-fn-in-trait lifetime desugaring holds in Phase 1 before Phase 3 consumes it; if it fights, fall back to a specific `impl BundleSink for &store::Store`.
- [ ] **Per-Work vs. per-Plan `JoinSet` scope.** Current draft puts one `JoinSet` on `DaemonContext`. Plans-scoped join-set (one per plan_id) gives cleaner "plan complete" semantics for Stage 8's Reviewer wiring. Deferred until Stage 8 forces the choice.
- [ ] **Where `ImplementerConfig` is loaded from.** The draft assumes it's composed into the top-level loopr config (vision line 208). Verify the existing `agents::config` module shape aligns before Phase 2.
- [ ] **Worktree reconcile ordering at daemon startup.** Draft asserts reconcile completes before socket-bind; confirm by reading `daemon/startup.rs` during Phase 2.
- [ ] **`FailureReason` variant for implementer errors.** Vision lists `FailureReason` as `{TokenBudget, ToolFailure, ReviewerRejection, AcUnmet, Panic, Other(String)}`. Stage 7 daemon maps most errors to `Other(String)` until the variants earn discrimination; revisit when a real run demands a more specific bucket.
- [ ] **Budget enforcement.** Vision specifies per-Work / per-run caps with soft-pause semantics. Not wired in Stage 7; earn when a runaway Implementer surfaces. Placeholder at `DaemonContext::can_spawn_new_work() -> bool { true }`.

## References

- `docs/vision.md`:
  - Lines 160–173 — `agents` ABI and `Deps<...>` pattern
  - Lines 118–141 — `tools` + `worktree` ABIs
  - Lines 311–326 — Worktree crash recovery (orchestrated in `loopr`)
  - Lines 408–419 — Error Model (panic posture, `FailureReason`)
  - Lines 602–611 — Explicitly Not in First Gate (serial works, no Director)
- `docs/roadmap.md` Stage 7 — the exit criterion this doc implements
- `crates/tools/docs/design/2026-04-21-tool-registry.md` — `Tool` trait + `tools::dispatch` (the callee)
- `docs/design/2026-04-21-worktree-lifecycle.md` — `Worktree::create`/`cleanup` (the per-work sandbox)
- `docs/design/2026-04-21-implementer.md` — `run_implementer` + `Deps<L,T,S,C>` (the consumer)
- `crates/agents/CLAUDE.md` — DI pattern rule, per-role scope boundaries
- `crates/agents/src/dispatch.rs:60` — `ToolExecutor` trait (where `RealTools` lives)
- `crates/agents/src/implementer.rs:51` — `impl BundleSink for store::Store` (pattern mirrored by `RealTools`)
- `crates/loopr/src/transport/handler.rs:107` — `handle_plan_create` (call-site edit)
- `crates/loopr/src/daemon/context.rs` — `DaemonContext` (field expansion site)
- Architect review (Gemini) Round 1, 2026-04-22 — identified the WorksStore persistence gap (Work transitions were in-memory only, no `update` method), the synchronous-worktree reactor-block risk, the incorrect "synchronous `rev_parse_head`" claim, and the missing explicit `AttemptCleanupPolicy` match. All four findings folded into Pass 4. `WorksStore::update` promoted from "implied" to a Phase-2 prerequisite; worktree ops wrapped in `spawn_blocking` per vision.md:134.
