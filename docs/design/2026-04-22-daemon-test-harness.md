# Design Document: Daemon Test Harness — In-Process LLM Injection

**Author:** Scott Idler
**Date:** 2026-04-22
**Status:** Implemented
**Review Passes Completed:** 5/5
**Crates touched:** loopr, llm, agents, decomposer

## Summary

The Stage 8 smoke test `crates/loopr/tests/stage_8_plan_to_tick.rs` cannot be written today because the daemon has no seam for injecting a stubbed `LlmClient`. The daemon always double-forks; the `AnthropicClient` is constructed inline inside the grandchild process and stored as a concrete `Arc<AnthropicClient>` in `DaemonContext`. This doc specifies an in-process daemon harness: generify `DaemonContext` over `L: LlmClient`, split `run_active_daemon` into a `build_context` constructor, a `serve_core` pipeline body (shared by tests and production), and a thin `serve` wrapper that adds signal handling for production only. Expose a `DaemonHandle` for programmatic shutdown from tests. Centralize the `ScriptedLlm` stub currently duplicated across three per-crate test files.

## Problem Statement

### Background

Stage 8 is Implemented as of 2026-04-22 — all pipeline wiring is in place. The wiring doc's exit criterion (`docs/design/2026-04-22-stage-8-wiring.md:479,578,610`) is a single integration test that exercises the full Plan → Tick happy path with stubbed Anthropic responses on a real-git tempdir. That test has been deferred because the necessary injection seam does not exist.

Stage 9 (first-gate E2E against a real rust-version target) sits downstream. Rule #1 of the v5 process ("one design doc at a time, motivated by a failing run") means Stage 9's design doc is captured after that run attempts. The smoke test is the cheap confidence-check that must land before the live run, so that failures during the live run can be distinguished between "daemon wiring is wrong" and "LLM produced unexpected output."

### Problem

The concrete dependency surfaces:

1. **`DaemonContext` hardcodes the backend.**
   ```rust
   // crates/loopr/src/daemon/context.rs:98
   pub llm: Arc<AnthropicClient>,
   ```
   Every consumer (`dispatch`, `spawn_implementer_for_work`, `spawn_reviewer_for_bundle`) reaches the client through `ctx.llm`; none can substitute a different impl.

2. **The client is built inline inside the grandchild.**
   ```rust
   // crates/loopr/src/daemon.rs:343
   let anthropic = AnthropicClient::new(config.llm.clone(), api_key)?;
   ```
   This is after the double-fork (`run_grandchild`), which calls `process::exit` on completion. The daemon cannot be run from a test process.

3. **`run_active_daemon` is private and monolithic.** It builds the client, constructs `DaemonContext`, runs the reconcile sweep, binds the socket, serves the accept loop, drains tasks, and closes the store — all in one function body (`crates/loopr/src/daemon.rs:312-480`).

4. **`ScriptedLlm` is duplicated.** Three near-identical test-only stubs exist today:
   - `crates/agents/tests/seam_implementer.rs`
   - `crates/agents/tests/seam_reviewer.rs`
   - `crates/decomposer/src/decompose/tests.rs`

   A cross-cutting integration test needs one stub that all three role crates can route through.

### Goals

- Make `DaemonContext` parameterizable over any `L: LlmClient + Send + Sync + 'static`, with `AnthropicClient` as the production instantiation and a stub as the test instantiation.
- Split `run_active_daemon` into three layers: `build_context` (construction), `serve_core` (shared pipeline body: reconcile, accept loop, drain, store close), and `serve` (production-only wrapper that adds signal handling around `serve_core`). Tests call `build_context` with a stub and invoke `serve_core` directly, skipping the fork, pid-file, telemetry-init, and signal-handler layers.
- Expose a `DaemonHandle` that lets a test request programmatic shutdown (no SIGTERM) and await completion.
- Centralize `ScriptedLlm` behind a `stub` feature on `crates/llm`, consumed via `[dev-dependencies]` by every test that needs it.
- Land `crates/loopr/tests/stage_8_plan_to_tick.rs` that drives Plan → Tick end-to-end with stubbed LLMs on a real-git tempdir and asserts `Work::Done`, `Bundle::Merged`, a row in `ticks.jsonl`.
- Make the harness general enough that each future integration test whose pipeline is serial-per-Work is mostly scripted-responses + assertions, not harness surgery. Concretely: reviewer ChangeRequested → Work `Blocked`; integrator retry / circuit-breaker under simulated `Store(Stale)`; `LlmError::Fatal` from any role driving escalation. Each should be a single test file against the same harness. Multi-Work-DAG tests (parallel implementer tasks on a shared `ScriptedLlm`) are explicitly out of scope — a FIFO queue cannot route concurrent pops by role, and adding routing belongs in a future doc motivated by a real test.

### Non-Goals

- Changing the IPC protocol, wire format, or any message variant.
- Touching any agent behavior (decomposer output, implementer ralph loop, reviewer verdict handling, integrator merge strategy).
- Replaying recorded Anthropic responses from disk — the stub is purely scripted, queued per-test.
- Removing the double-fork or pid-file mechanics from the production path.
- Feature-gating any production behavior. The `stub` feature on `llm` is test-only.
- Emitting `DaemonEvent`s from the broadcast channel as part of pipeline completion — poll-based assertions on the store are sufficient for the smoke test.

## Proposed Solution

### Overview

Three changes, sequenced:

1. **Generify `DaemonContext<L>`.** Swap the `Arc<AnthropicClient>` field for `Arc<L>` where `L: LlmClient + Send + Sync + 'static`. Propagate the parameter through `dispatch`, `accept_loop`, `spawn_signal_watcher`, and the three drain functions.

2. **Split `run_active_daemon` into `build_context` + `serve_core` + `serve`.** Production's `daemon_main` chains `build_context` → `serve` (which adds a signal watcher, then calls `serve_core`). Tests call `build_context` with a stub and invoke `serve_core` directly, bypassing the signal watcher.

3. **Centralize `ScriptedLlm`** in `crates/llm/src/stub.rs` under a `stub` cargo feature, delete the three duplicates.

### Architecture

**Before (production, also the only path):**
```
main ─> lib::run ─> ensure_daemon (parent-side, pre-telemetry)
                         │
                         └──▶ double_fork ─▶ run_grandchild (pid=N)
                                                │
                                                └──▶ tokio::Runtime ─▶ daemon_main
                                                                         │
                                                                         ├─ Phase A: pid/version/run-id sentinels
                                                                         │
                                                                         └─ run_active_daemon (private):
                                                                              init telemetry
                                                                              open store
                                                                              build AnthropicClient    ◀── inlined
                                                                              build router+denylist
                                                                              build DaemonContext      ◀── concrete
                                                                              startup::reconcile
                                                                              bind Unix socket
                                                                              accept_loop
                                                                              drain tasks
                                                                              close store
                                                                              process::exit
```

**After — production path:**
```
main ─> lib::run ─> ensure_daemon (parent, pre-telemetry)
            │
            └─▶ fork ─▶ daemon_main
                         │
                         ├─ Phase A: pid/version/run-id sentinels
                         │
                         └─ run_active_daemon:
                              init telemetry
                              build Config + AnthropicClient
                              build_context(target, run_id, pid, anthropic, cfg)    ◀── new
                              serve(ctx)                                            ◀── new; installs signal watcher
                              process::exit
```

**After — test path:**
```
#[tokio::test]
let scripted = ScriptedLlm::new();
scripted.queue_tool(...);      // decomposer response
scripted.queue_free(...);      // implementer response
scripted.queue_free(...);      // reviewer response

let test_daemon = spawn_test_daemon(scripted).await;
// spawn_test_daemon internally:
//   1. TempDir::new()
//   2. common::init_git_repo(&tempdir)           (empty initial commit)
//   3. Config::default()
//   4. ctx = build_context(tempdir, run_id, 0, scripted, cfg).await?
//   5. handle = DaemonHandle::from_context(&ctx)        ◀── computes socket path
//   6. task = tokio::spawn(async move { serve_core(ctx).await })
//   7. wait for handle.socket_path() to exist
//   8. return TestDaemon { handle, target, task, _tempdir }

// Test body:
let client = ipc_client_connect(test_daemon.handle.socket_path()).await?;
client.handshake().await?;
let plan = client.plan_create("...").await?;

// Race poll success, fast-fail on Blocked, surface daemon-task panic if any:
tokio::select! {
    result = poll_jsonl_until_done_or_blocked(&target, &plan.id, deadline) => result?,
    join = &mut test_daemon.task => panic!("daemon exited early: {join:?}"),
}
// Read the final JSONL snapshots for assertions:
assert_eq!(jsonl_last_work(&target, &work_id).status, WorkStatus::Done);
assert_eq!(jsonl_last_bundle(&target, &bundle_id).status, BundleStatus::Merged);

// Shutdown consumes TestDaemon → the only compilable path is clean teardown:
test_daemon.shutdown().await?;  // internally: handle.shutdown(); task.await?; try_unwrap; store.close().await
```

`serve_core` is the new shared pipeline body; `serve` is the production wrapper that adds signal handling + `try_unwrap` + `store.close()`. Both do everything `run_active_daemon` does *except* allocate the run-id, write the pid file, and init telemetry — those are fork-layer / process-layer concerns the test does not want. `serve_core` returns the `Arc<DaemonContext<L>>` so the caller (production `serve` or test harness) handles `try_unwrap` + store close *after* joining any additional Arc holders.

### Data Model

**`DaemonContext<L>`** (generic; no functional change beyond the type parameter):

```rust
pub struct DaemonContext<L: LlmClient + Send + Sync + 'static> {
    pub target: PathBuf,
    pub run_id: RunId,
    pub started_at: chrono::DateTime<chrono::Local>,
    pub pid: u32,
    pub events: broadcast::Sender<DaemonEvent>,
    pub shutting_down: Arc<AtomicBool>,
    pub shutdown_notify: Arc<Notify>,
    pub store: Store,
    pub llm: Arc<L>,                       // ◀── was Arc<AnthropicClient>
    pub router: Arc<LaneRouter>,
    pub bash_denylist: Arc<BashDenylist>,
    pub path_deny_patterns: Vec<String>,
    pub sandbox: SandboxMode,
    pub context_builder: Arc<InlineContextBuilder>,
    pub implementer_config: ImplementerConfig,
    pub reviewer_config: ReviewerConfig,
    pub integrator_config: IntegratorConfig,
    pub git_lock: Arc<Mutex<()>>,
    pub worktree_cleanup_policy: AttemptCleanupPolicy,
    pub implementer_tasks: Mutex<JoinSet<()>>,
    pub reviewer_tasks: Mutex<JoinSet<()>>,
    pub integrator_tasks: Mutex<JoinSet<()>>,
}
```

**`DaemonHandle`** (new; test-facing, also usable by future graceful-shutdown RPCs):

```rust
pub struct DaemonHandle {
    shutting_down: Arc<AtomicBool>,
    shutdown_notify: Arc<Notify>,
    socket_path: PathBuf,
}

impl DaemonHandle {
    /// Clone the shutdown atomics and derive the socket path via
    /// `sentinel::socket_path(&ctx.target)` so the handle's view of the
    /// socket location matches where `serve_core` binds the listener.
    pub fn from_context<L: LlmClient + Send + Sync + 'static>(
        ctx: &Arc<DaemonContext<L>>,
    ) -> Self { /* socket_path = sentinel::socket_path(&ctx.target) */ }

    pub fn socket_path(&self) -> &Path { &self.socket_path }

    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Relaxed);
        self.shutdown_notify.notify_waiters();
    }
}
```

**`ScriptedLlm`** (new, moved from three per-crate duplicates):

```rust
// crates/llm/src/stub.rs  (gated: #[cfg(feature = "stub")])
//
// Queues live behind Arc<Mutex<_>> and the struct is Clone, so a test can
// clone the stub BEFORE handing it to spawn_test_daemon and keep a probe
// handle for post-run assertions (queue-drained checks, unused-response
// inspection). Without Clone + Arc, the daemon consumes the stub by value
// and the test loses visibility into queue state after shutdown.
#[derive(Clone)]
pub struct ScriptedLlm {
    tool_responses: Arc<Mutex<VecDeque<Result<ToolCall, LlmError>>>>,
    free_responses: Arc<Mutex<VecDeque<Result<String, LlmError>>>>,
}

impl ScriptedLlm {
    pub fn new() -> Self { /* empty queues */ }
    pub fn queue_tool(&self, result: Result<ToolCall, LlmError>) { ... }
    pub fn queue_free(&self, result: Result<String, LlmError>) { ... }
    /// True when both queues are empty. Call after shutdown to assert
    /// the test drove every scripted response.
    pub fn is_empty(&self) -> bool { ... }
    /// Returns the remaining queue lengths, useful for diagnostic output
    /// when an assertion fails mid-test.
    pub fn remaining(&self) -> (usize, usize) { ... }
}

impl LlmClient for ScriptedLlm { /* pop queue; panic-with-diagnostic if empty */ }
```

Test pattern:
```rust
let scripted = ScriptedLlm::new();
scripted.queue_tool(...);
scripted.queue_free(...);
scripted.queue_free(...);
let probe = scripted.clone();              // shares the Arc<Mutex<_>> queues
let daemon = spawn_test_daemon(scripted).await;
// ... test body ...
daemon.shutdown().await?;
assert!(probe.is_empty(), "unused scripted responses: {:?}", probe.remaining());
```

### API Design

**New public items in `crates/loopr/src/daemon.rs`** (or submodule). The shared pipeline body splits into three layers so the test harness can opt out of signal handling without losing the accept loop and shutdown drain.

```rust
// Layer 1: pure construction. No I/O beyond store-open and tool-infra build.
// Callers own the returned Arc; hand out DaemonHandle::from_context before
// spawning serve_core so shutdown can be requested programmatically.
pub async fn build_context<L>(
    target: PathBuf,
    run_id: RunId,
    pid: u32,
    llm: L,
    config: Config,
) -> Result<Arc<DaemonContext<L>>, LooprError>
where
    L: LlmClient + Send + Sync + 'static;

// Layer 2: pipeline body. Runs reconcile, binds socket, runs accept_loop,
// drains task pools. NO signal handlers installed — shutdown is driven
// exclusively by ctx.shutting_down + ctx.shutdown_notify.
//
// Returns the Arc back to the caller so try_unwrap + store.close() happen
// at the outer layer, AFTER the caller has joined any other Arc holders
// (e.g. production's signal watcher). This is the fix for the
// watcher-drop ordering bug that a naïve "serve_core closes the store"
// split would introduce: if the watcher still holds a clone when
// Arc::try_unwrap runs, it deterministically fails in production.
pub async fn serve_core<L>(
    ctx: Arc<DaemonContext<L>>,
) -> Result<Arc<DaemonContext<L>>, LooprError>
where
    L: LlmClient + Send + Sync + 'static;

// Layer 3: production wrapper. Installs SIGTERM/SIGINT watcher, calls
// serve_core, awaits the returned Arc, joins the watcher so its Arc
// clone drops, THEN try_unwraps and closes the store. Not used by
// tests (would collide with the test runner's signal handlers).
pub async fn serve<L>(
    ctx: Arc<DaemonContext<L>>,
) -> Result<(), LooprError>
where
    L: LlmClient + Send + Sync + 'static;
```

**Production path becomes** (replaces the inline body of `run_active_daemon`):

```rust
async fn run_active_daemon(target: PathBuf, run_id: RunId, pid: u32) -> Result<(), LooprError> {
    let _guard = telemetry::init(&target, &run_id, &directive)?;
    let config = Config::load(&target)?;
    let api_key = resolve_api_key(&config.llm);
    let anthropic = AnthropicClient::new(config.llm.clone(), api_key)?;
    let ctx = build_context(target, run_id, pid, anthropic, config).await?;
    serve(ctx).await      // wraps serve_core + watcher + try_unwrap + store.close
}
```

**Shutdown ownership contract** (preserved from today's `run_active_daemon`, moved up one layer):

- `serve_core` returns `Arc<DaemonContext<L>>` with the pipeline drained and the accept loop exited. Task-pool drains inside `serve_core` guarantee that handler/implementer/reviewer/integrator clones of the Arc have dropped.
- The caller (production `serve`, or the test harness's shutdown path) is responsible for joining any *additional* Arc holders it spawned alongside `serve_core`. In production, that's the signal watcher: `tokio::time::timeout(WATCHER_JOIN_TIMEOUT_SECS, watcher_handle)`. In tests, there are none.
- With all clones released, the caller runs `Arc::try_unwrap(ctx)`. On success: `owned.store.close().await`. On failure (stranded clone bug): log `strong_count`, fall back to the store's sync `Drop`. This mirrors today's `daemon.rs:465-476` verbatim.

**Test entry point** (in `crates/loopr/tests/common/mod.rs` or a new `common/harness.rs`):

```rust
pub struct TestDaemon<L: LlmClient + Send + Sync + 'static> {
    pub handle: DaemonHandle,
    pub target: PathBuf,        // owned via TempDir keep-alive
    pub task: tokio::task::JoinHandle<Result<(), LooprError>>,
    _tempdir: TempDir,
    _ctx_marker: PhantomData<L>,
}

pub async fn spawn_test_daemon<L>(llm: L) -> TestDaemon<L>
where
    L: LlmClient + Send + Sync + 'static;
```

**Signature propagation.** The generic parameter `L` threads through the following sites. Every one is a purely mechanical type-parameter addition; no logic changes.

| File | Line(s) | Function |
|---|---|---|
| `daemon/context.rs` | 49 | `struct DaemonContext` — field `llm: Arc<L>` |
| `daemon/context.rs` | 169 | `DaemonContext::new` |
| `daemon/context.rs` | 241 | `spawn_implementer_for_work` |
| `daemon/context.rs` | 380 | `spawn_reviewer_for_bundle` |
| `daemon/context.rs` | 522 | `spawn_integrator_for_bundle` |
| `daemon/context.rs` | 641 | `tool_context` (no body use; impl block only) |
| `daemon.rs` | 81 | `drain_implementer_tasks` |
| `daemon.rs` | 106 | `drain_reviewer_tasks` |
| `daemon.rs` | 131 | `drain_integrator_tasks` |
| `daemon.rs` | 487 | `spawn_signal_watcher` |
| `transport/handler.rs` | 33 | `dispatch` |
| `transport/handler.rs` | 94 | `handle_status` |
| `transport/handler.rs` | 109 | `handle_plan_create` |
| `transport/handler.rs` | 176 | `handle_plan_list` |
| `transport/server.rs` | `accept_loop` (and whatever it spawns into the handler `JoinSet`) — line per read at Phase 2 |
| `daemon/startup.rs` | `reconcile` (currently takes `&Arc<DaemonContext>`) — line per read at Phase 2 |

### Implementation Plan

**Phase 1 — `ScriptedLlm` centralization.** Pure move + dedup, no daemon changes. Runs first for two reasons: (a) it validates the stub's generalized API against three existing consumers before the Stage 8 smoke test depends on it, and (b) it keeps each phase independently shippable — if Phases 2–5 slip, the dedup still stands on its own merit.

- Add `stub` feature to `crates/llm/Cargo.toml`.
- Create `crates/llm/src/stub.rs` with `ScriptedLlm` (the seam-test shape, generalized over both trait methods).
- `#[cfg(feature = "stub")] pub use stub::ScriptedLlm;` in `lib.rs`.
- Update `crates/agents/Cargo.toml`, `crates/decomposer/Cargo.toml`: `llm = { workspace = true, features = ["stub"] }` in `[dev-dependencies]`.
- Delete the three `ScriptedLlm`/`MockLlmClient` duplicates; replace their `use` lines.
- `otto ci` green at repo root.

**Phase 2 — `DaemonContext<L>` generification.** Type-parameter only; no behavior change.

- Add `<L: LlmClient + Send + Sync + 'static>` to `DaemonContext`, its `impl` blocks, and every consumer named above.
- Update the production call site in `run_active_daemon` to pass `AnthropicClient` explicitly.
- `otto ci` green.

**Phase 3 — Extract `build_context` + `serve`.** Split the Phase B body.

- Move the construction sequence (store open, config load, router/denylist build, `DaemonContext::new`, reconcile) into `build_context`.
- Move the accept-loop + drain sequence into `serve_core`, which returns the `Arc<DaemonContext<L>>` (not `()`).
- Add the production `serve` wrapper: install signal watcher, call `serve_core`, await the returned Arc, join watcher, `try_unwrap` + `store.close()`.
- `run_active_daemon` becomes a three-line wrapper.
- `otto ci` green; `smoke.rs` and `daemon.rs` integration tests remain the authoritative coverage for the production shutdown sequence.

**Phase 4 — `DaemonHandle` + test harness.** New surface.

- Add `DaemonHandle` in `crates/loopr/src/daemon/handle.rs`.
- Add `common/harness.rs` in `crates/loopr/tests/`.
- Smoke-verify the harness via a tiny test that starts and immediately shuts down a daemon (no LLM calls exercised).
- `otto ci` green.

**Phase 5 — `stage_8_plan_to_tick.rs`.** The payoff.

- Init real git in a `TempDir` via existing `common::init_git_repo`.
- Construct `ScriptedLlm` with queued decomposer tool-call response, implementer free-form response, reviewer free-form Accept verdict. The implementer's scripted response writes to a **top-level file** (e.g. `VERSION.md`) so no `mkdir -p` is required — the bare `git init` tempdir has no `src/` directory, and the test must not assume one.
- `spawn_test_daemon(scripted).await`.
- Open an IPC client against `handle.socket_path()`, send handshake + `plan.create`, receive `PlanCreateResult`.
- **Polling strategy: JSONL reads, not a second `Store` handle.** `.loopr/taskstore/{works,bundles,ticks}.jsonl` are append-only; the test parses each line with `serde_json::from_str::<Work>` / `Bundle` / `Tick` on every poll. This avoids any question about concurrent in-process `AsyncStore` handles contending on SQLite or the taskstore writer thread, and makes the test's read path a strict subset of the daemon's persistence contract.
- **Fail-fast via `tokio::select!`.** The poll loop races three futures: (a) success (`Work.status == Done` AND `Tick` row present); (b) fast-fail (`Work.status == Blocked`, surface a pointer to `<target>/.loopr/runs/<run-id>/events.log`); (c) the daemon task itself (`&mut daemon_task`) — if it completes first, propagate its `JoinError` so any panic inside a spawned implementer/reviewer/integrator task lands in the test output instead of timing out silently. Deadline caps the whole race.
- Assert `Work.status == Done` and `Bundle.status == Merged` from the last JSONL snapshot.
- `handle.shutdown().await`; `task.await??` yields the drained `Arc<DaemonContext<ScriptedLlm>>`; test-side `Arc::try_unwrap` + `owned.store.close().await` closes the store cleanly before the tempdir drops.
- `otto ci` green; test is Stage 8's exit criterion.

## Alternatives Considered

### Alternative 1: `dyn LlmClient` + type erasure (rejected)

Two variants of this alternative surfaced during review.

**1a. Direct `Arc<dyn LlmClient>` in `DaemonContext`.**
- **Description:** Change `ctx.llm` to `Arc<dyn LlmClient>`; modify the trait in `crates/llm/src/client.rs` to be object-safe.
- **Why not chosen:** `LlmClient` uses `impl Future<...> + Send + 'a` returns so the `Send` bound is explicit at the call site (required for `tokio::spawn`-scoped tasks). Making the trait dyn-compatible forces `Pin<Box<dyn Future + Send>>` returns, which erases the bound and requires re-asserting it at every adapter. This undoes `llm`'s scope memo U+4 decision directly.

**1b. Local `DynLlm` adapter inside `crates/loopr` only.**
- **Description:** Keep `crates/llm`'s generic trait unchanged. Add a private `DynLlm` trait inside `crates/loopr/src/daemon/` with `Pin<Box<dyn Future + Send>>` returns and a blanket impl for any `L: LlmClient + Send + Sync + 'static`. `DaemonContext` holds `Arc<dyn DynLlm>` — concrete type, no generic parameter.
- **Pros:** `DaemonContext`, `dispatch`, `accept_loop`, `reconcile`, and the task-drain functions all stay concrete. No generic-parameter propagation across the daemon's non-LLM-aware code.
- **Cons:** Adds an adapter layer whose sole purpose is to erase a type that the existing trait explicitly refuses to erase. A reader encountering `DynLlm` has to understand *why* `loopr` has a second trait shape when `LlmClient` already exists, why it lives in `loopr` and not `llm`, and why the blanket impl forwards to the generic trait. Cost: one explanation-debt layer. One heap allocation per LLM call (noise against a network RPC, but non-zero for no architectural gain). Violates `rust.md`'s "use generics for DI, never `dyn` trait objects or `Box<dyn ...>`" rule.
- **Why not chosen:** the concern the adapter is meant to address — "generic contagion" — is bounded. The generic parameter propagates through one crate (`loopr`), touching ~15 already-identified functions in one module tree (`daemon/` + `transport/`). Every site is mechanical, `cargo check`-verified, and the parameter is inert once added. Compile-time and rust-analyzer costs are small-integer monomorphizations contained to one crate. The adapter's "less signature noise" win is paid for by a "more abstraction to justify" cost; the generic approach is both the rule-consistent choice and the simpler shape once a reader knows the codebase uses generics end-to-end.

### Alternative 2: `LlmBackend` enum

- **Description:** Concrete enum `LlmBackend { Anthropic(AnthropicClient), Scripted(ScriptedLlm) }` implementing `LlmClient` via match-based forwarding. `DaemonContext` holds `Arc<LlmBackend>`.
- **Pros:** No generics, no `dyn`; one concrete type swaps at startup based on config or test call.
- **Cons:** `LlmBackend` has to live in a crate that sees both variants, which forces either (a) putting `ScriptedLlm` in the production dependency graph or (b) feature-gating the variant. (a) breaks the "stub is test-only" invariant; (b) makes the enum's variant set depend on compilation features, which is subtly worse than today's concrete type. Every future backend (recorded-replay, cache, advisor-fanout) must modify the enum.
- **Why not chosen:** closed-set polymorphism when the stage crates already speak open-set trait generics. Open-set is the right shape; closed-set would be a regression from the existing `LlmClient` design.

### Alternative 3: Config-driven stub loader

- **Description:** Add a `llm.stub-responses-path` field to `.loopr/config.yml`; daemon reads it at startup and constructs `ScriptedLlm` from disk. Test writes config + responses file, invokes daemon via real `assert_cmd`, asserts via store reads after the daemon exits.
- **Pros:** Smallest architectural change; keeps the forked-daemon pattern identical; no generics or refactor.
- **Cons:** Introduces a production config field whose only purpose is to degrade the daemon into test mode. The config-file shape becomes a persistent test-mode backdoor. Scripted responses as on-disk JSON is harder to read and maintain than Rust source. Tests still pay the fork + IPC + process-exit cost on every run. Zero value for future tests that want finer control (e.g., injecting a failing tool mid-ralph-loop, inspecting mid-pipeline state).
- **Why not chosen:** solves the Stage 8 smoke test at the cost of a test-shaped hole in production config. Option 2 (this doc) is the long-term investment; option 3 was explicitly presented to the user and declined.

### Alternative 4: Bypass the daemon entirely

- **Description:** Drive `handle_plan_create` from test code directly; manually construct `DaemonContext`, skip the accept loop and IPC entirely.
- **Pros:** No refactor of `run_active_daemon`; smallest code delta.
- **Cons:** Does not exercise the IPC layer, handshake, dispatch, or accept-loop wiring — exactly the surface Stage 8 is meant to validate. A passing test proves nothing about whether the daemon actually accepts a `plan.create` over the wire.
- **Why not chosen:** defeats the purpose of the smoke test. Stage 8's wiring doc explicitly enumerates the full in-process pipeline including the accept loop (`docs/design/2026-04-22-stage-8-wiring.md:578`).

## Technical Considerations

### Dependencies

- **`crates/llm`**: gains a `stub` feature and `stub.rs` module. No new external deps.
- **`crates/loopr`**: gains an `llm = { workspace = true, features = ["stub"] }` entry in `[dev-dependencies]`. No new external deps for production.
- **`crates/agents`, `crates/decomposer`**: switch their dev-only `ScriptedLlm` imports to `llm::ScriptedLlm`. No production diff.
- Workspace: no new crates.

### Performance

No production performance impact. The generic parameter is monomorphized per backend; the only backend in production is `AnthropicClient`, identical codegen to today. Compile-time impact is bounded: `DaemonContext<L>` instantiates once for production (`AnthropicClient`) and once per test binary that uses the stub (`ScriptedLlm`) — small-integer monomorphizations, contained to one crate. rust-analyzer pays a modest cost on `crates/loopr/src/daemon/`'s ~15 generic signatures; acceptable against the alternative of rewriting the trait contract. The test path gains an extra in-process tokio task (the daemon's `serve_core` future) but is bounded by `stage_8_plan_to_tick` only.

### Security

The `stub` feature on `crates/llm` is a supply-chain concern only if a downstream binary accidentally enables it. Mitigation: the feature is declared only in `[dev-dependencies]` sections across the workspace; the production `loopr` binary's `[dependencies]` entry for `llm` does not list `stub`. Cargo's feature unification will not pull it in unless a consumer explicitly asks for it.

### Testing Strategy

- **Phase 1 verification:** all existing seam tests in `crates/agents/tests/` and `crates/decomposer/` continue to pass after `ScriptedLlm` is moved, proving the generalization is behavior-preserving.
- **Phase 2 verification:** `cargo check --workspace` and `otto ci` pass — pure type-parameter churn must not change runtime behavior.
- **Phase 4 verification:** a minimal harness smoke test ("start daemon, shut it down, no LLM calls") proves the in-process path works before any scripted traffic.
- **Phase 5 verification:** `stage_8_plan_to_tick.rs` produces a `ticks.jsonl` row with a two-parent merge commit visible via `git log` on the integration branch, and the assertions on Work and Bundle status pass.
- **Coverage boundaries.** The new smoke test covers the full pipeline wiring end-to-end but does **not** exercise the production shutdown sequence (signal watcher install, watcher-drop ordering around `Arc::try_unwrap`). That coverage remains with the existing `smoke.rs` and `daemon.rs` integration tests, which fork real daemons over real IPC and drive SIGTERM shutdowns. The two test families are complementary: smoke+daemon tests own the process-lifecycle surface, stage_8 owns the pipeline-wiring surface.

### Rollout Plan

Each phase is its own commit. `otto ci` is green at the end of every commit. No feature flags, no dual paths. Paradigm change, not a migration.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Generics propagation touches more files than anticipated, causing a churn-heavy PR | Med | Low | Phase 2 is type-parameter-only; any file hit is a mechanical add. A generics-heavy PR is still just a single commit in v5 (rule #3). |
| `ScriptedLlm` semantics differ across the three existing duplicates, and consolidating hides a test-local assumption | Med | Med | Before deletion, diff all three and call out any behavior that varies. Promote the richest shape; adjust consumers. |
| `serve_core` in a test process leaks tokio tasks if the harness's `Drop` forgets to shut down | Low | Med | `TestDaemon::shutdown(self)` **consumes `self`** — the only path to clean teardown is explicit. `Drop` cannot call `block_on` (already inside a tokio runtime, would panic with "cannot start a runtime from within a runtime"); the safe fallback is to abort the task handle and emit a loud `eprintln!` so forgotten shutdowns fail the test via dropped output rather than silently leaking. |
| Test-time daemon race: IPC client connects before socket binds | Low | Low | `spawn_test_daemon` waits for the socket file to appear (same poll as `wait_for_socket` in `daemon.rs:198`) before returning. |
| `DaemonContext` generic parameter turns every trait bound into a Rust-analyzer complaint | Med | Low | All bounds (`LlmClient + Send + Sync + 'static`) consolidate into a single type alias or a `where`-clause macro if readability degrades. First pass: spell them out, cosmeticize only if painful. |
| Scripted response queue exhausts mid-test, producing an unhelpful panic | Med | Low | `ScriptedLlm` panics with the queue state and the method name so failures are diagnosable; tests queue one extra no-op response or assert the queue is empty at test end. |
| `TempDir` drops while daemon task is still draining in-flight writes, causing ENOENT on pending I/O | Low | Med | `TestDaemon` field order places `_tempdir` last so it drops after the explicit shutdown path has already awaited `task`. `shutdown(self)` consuming `self` makes this ordering the only compilable path. |
| Test runner receives a real SIGTERM / SIGINT while `serve_core` is running, bypassing `handle.shutdown()` | Low | Low | `serve_core` deliberately omits signal handlers; the test process inherits the test runner's handlers (typically test-framework-level, not a leak). Production's `serve` is the only path that installs them. |
| A test clones `Arc<DaemonContext<L>>` (e.g., to poke at `ctx.store` before spawning `serve_core`), stranding the clone past `Arc::try_unwrap` at shutdown and triggering `Store::Drop` on the tokio runtime | Med | High | `spawn_test_daemon` returns only a `TestDaemon` value that wraps `DaemonHandle` (atomics only, not the context Arc) and the task's `JoinHandle`; the context Arc is moved into the spawned task and returned by it at completion. Tests that want to read the store use JSONL file reads — they must not pull on `ctx.store`. Called out in the harness's rustdoc. |
| Watcher-drop ordering regresses during the `serve` split, causing tokio panics on shutdown in production | Low | High | `serve_core` returns the Arc rather than closing the store itself, so watcher-join happens in the caller BEFORE `try_unwrap` — the same ordering as today's `daemon.rs:456-476`, re-expressed across two layers. `smoke.rs` and `daemon.rs` integration tests (which fork real daemons and exercise the full shutdown) remain the authoritative regression guard; the new smoke test covers pipeline wiring but does not exercise signal-driven shutdown. |
| Panic inside a spawned agent task (e.g., `ScriptedLlm` empty-queue panic in `spawn_implementer_for_work`) is swallowed by the `JoinSet` and the test's poll loop times out with no diagnostic | Med | Med | The test's poll loop races against `&mut daemon_task` via `tokio::select!`; if the daemon task completes early, its `JoinError` surfaces to the test output with the panic trace. `ScriptedLlm` panics with queue-state + method name so the trace is actionable. |

## Open Questions

- [ ] Does `startup::reconcile(&ctx)` need any signature change beyond the generic parameter? (Likely not; it already takes `&Arc<DaemonContext>`.)
- [ ] Should `DaemonHandle` also provide an `await_shutdown()` so the caller can block until the daemon has drained, separate from requesting shutdown? (Useful for graceful-shutdown RPCs later; first pass can skip.)
- [ ] Where should the test harness type `TestDaemon` live — in `crates/loopr/tests/common/` (sibling to existing helpers) or elevated to `crates/loopr/src/test_harness.rs` with `#[doc(hidden)] pub`? First pass: tests-local, since no other crate consumes it yet.

**Resolved during Pass 2:**
- `build_context` takes a fully-constructed `Config`. Production builds it via `Config::load(&target)`; tests pass `Config::default()` with no `.loopr/config.yml` required on disk.

**Resolved post-Architect review:**
- `serve_core` returns `Arc<DaemonContext<L>>` (not `()`). `Arc::try_unwrap` + `store.close().await` move to the outer caller (production `serve` or test harness), so watcher-join happens before `try_unwrap` in production and there is no ordering bug.
- Test assertion path reads `.loopr/taskstore/{works,bundles,ticks}.jsonl` directly via `serde_json::from_str`. No second `Store` handle. Avoids any question about multi-handle `AsyncStore` opens.
- `TestDaemon::shutdown(self)` consumes `self`; `Drop` cannot `block_on` from inside a tokio runtime, so the only safe path is explicit shutdown. Drop aborts + prints a loud warning if the test forgot.
- Poll loop in `stage_8_plan_to_tick.rs` races `Done OR Blocked` against the daemon task itself via `tokio::select!`, so an unexpected panic inside a spawned agent task surfaces as a `JoinError` in the test output instead of a silent timeout.
- Multi-Work DAG tests are explicitly out of scope for this harness: a single FIFO `ScriptedLlm` cannot route concurrent pops by role. A future doc adds routing when a real multi-Work test motivates it.

## References

- `docs/design/2026-04-22-stage-8-wiring.md` — the wiring capstone whose exit criterion this doc unblocks.
- `docs/roadmap.md` Stage 8 / Stage 9 — the gate this test precedes.
- `docs/vision.md` — architectural shape; generics-over-`dyn` for DI; process rules.
- `crates/loopr/src/daemon.rs:312-480` — the monolithic `run_active_daemon` this doc splits.
- `crates/loopr/src/daemon/context.rs:49-212` — `DaemonContext` definition.
- `crates/loopr/src/transport/handler.rs:109-174` — `handle_plan_create` dispatch.
- `crates/llm/src/client.rs` — the `LlmClient` trait shape this doc preserves.
- `crates/agents/tests/seam_implementer.rs`, `seam_reviewer.rs`, `crates/decomposer/src/decompose/tests.rs` — the three `ScriptedLlm`/`MockLlmClient` duplicates this doc centralizes.
