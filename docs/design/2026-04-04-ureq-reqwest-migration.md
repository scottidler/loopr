# Design Document: ureq to reqwest Async Migration

**Author:** Scott A. Idler
**Date:** 2026-04-04
**Status:** Approved
**Review Passes Completed:** 5/5

## Summary

Surgically remove `ureq` (synchronous HTTP client) and replace all usage with `reqwest` (async). The migration cascades async from the HTTP trait up through LlmClient, DocValidator, CoverageEvaluator, Decomposer, IPC handlers, dispatch, and the IPC server closure. Each step targets ONE file, runs `otto check`, and fixes all resulting errors before proceeding.

## Problem Statement

### Background

The Doc Validator, Coverage Evaluator, and Decomposer make blocking `ureq` LLM calls from inside a Tokio async context. The agent path already uses `reqwest` (async). Having two HTTP clients is unnecessary, and the sync calls block Tokio worker threads during decomposition (dozens of sequential 30-120s LLM calls).

### Problem

Synchronous `ureq` calls inside Tokio tasks block threads for the full duration. This risks exhausting the thread pool during decomposition and delays all concurrent IPC handlers. Having both `ureq` and `reqwest` in the dependency tree is wasteful.

### Prior Attempt (FAILED)

A previous attempt to do this migration devolved into a cascading shitshow of 37 Python fix scripts because changes were made across too many files simultaneously. Compilation errors compounded faster than they could be resolved. The entire branch was discarded.

### Lessons Learned - Execution Discipline

1. **ONE file at a time.** Never edit a second file until the first one compiles (with expected downstream errors only).
2. **`otto check` after every file change.** Read every error. Understand what broke and why.
3. **Fix forward, not sideways.** Each step fixes the compilation errors introduced by the PREVIOUS step. Never introduce new architectural changes while fixing cascade errors.
4. **No helper scripts.** All changes via Edit tool. If you need a Python script to fix your Rust code, you've lost control.
5. **The cascade is predictable.** Each async function breaks its callers. The callers are known in advance. The fix is mechanical: add `async`, add `.await`, add `#[tokio::test]`.

### Goals

- Remove `ureq` dependency entirely
- Replace all sync HTTP calls with async `reqwest`
- Propagate `async fn` up the full call chain
- Make `dispatch` async and update the IPC server handler signature
- Every intermediate step (after fixing cascade errors) compiles via `otto check`

### Non-Goals

- Parallelizing LLM calls inside the decomposer (sequential order is load-bearing)
- Changing the agent LLM path (already uses reqwest/SSE)
- Changing retry logic, model config, prompt construction, or domain types
- Refactoring beyond what the async migration strictly requires

## Proposed Solution

### Architecture - Call Chain Before and After

Before:

```
handle_client (async, closure: Fn(req) -> DaemonResponse)
  dispatch (sync fn)                              [daemon/handlers.rs:110]
    handle_doc_accept (sync)                      [daemon/handlers/doc.rs:52]
      accept_plan_markdown (sync)                 [daemon/handlers/doc.rs:160]
        classify_brief (sync)                     [daemon/handlers/doc.rs:455]
          LlmClient::call (sync)                  [validator/client.rs:75]
            HttpClient::post (sync) <- ureq       [validator/client.rs:16]
        decompose_hierarchy (sync)                [decomposer.rs:476]
          call_llm_for_children (sync)            [decomposer.rs:176]
            HttpClient::post (sync) <- ureq
    handle_validator_validate (sync)              [daemon/handlers/integrator.rs:265]
      DocValidator::validate_plan (sync)          [validator.rs:49]
        LlmClient::call_with_retry (sync)         [validator/client.rs:131]
    handle_coverage_evaluate (sync)               [daemon/handlers/integrator.rs:369]
      CoverageEvaluator::evaluate_* (sync)        [evaluator.rs:67]
        LlmClient::call_with_retry (sync)
```

After:

```
handle_client (async, closure: Fn(req) -> BoxFuture<DaemonResponse>)
  dispatch (async fn)
    handle_doc_accept (async)
      accept_plan_markdown (async)
        classify_brief (async)
          LlmClient::call (async)
            HttpClient::post (async) <- reqwest
        decompose_hierarchy (async)
          call_llm_for_children (async)
            HttpClient::post (async) <- reqwest
    handle_validator_validate (async)
      DocValidator::validate_plan (async)
        LlmClient::call_with_retry (async)
    handle_coverage_evaluate (async)
      CoverageEvaluator::evaluate_* (async)
        LlmClient::call_with_retry (async)
```

### Data Model

No changes to persisted types.

## Implementation Plan - Step by Step

The cascade propagates UPWARD. We start at the bottom (HTTP client) and fix one file at a time, moving up the call chain. After each step, run `otto check` and fix all errors in that file before proceeding.

---

### Step 1: `src/validator/client.rs` - Make HttpClient trait async, replace UreqClient

**Changes:**

1. Add `use async_trait::async_trait;` to imports
2. Add `#[async_trait]` to `HttpClient` trait, make `fn post` -> `async fn post`
3. Delete `UreqClient` struct and its `impl HttpClient`
4. Create `ReqwestClient` struct with `reqwest::Client` field
5. Implement `#[async_trait] impl HttpClient for ReqwestClient` using reqwest async
6. Rename `LlmClient::with_ureq` to `LlmClient::with_reqwest`, construct `ReqwestClient` inside
7. Make `LlmClient::call` -> `pub async fn call`; add `.await` to `self.http_client.post` call
8. Make `LlmClient::call_with_retry` -> `pub async fn call_with_retry`; add `.await` to `self.call` calls
9. Tests: add `#[async_trait]` to `MockHttpClient`, `RecordingHttpClient`, `FailingHttpClient`; make their `fn post` -> `async fn post`
10. Tests: change `#[test] fn` -> `#[tokio::test] async fn`; add `.await` to `client.call()` and `client.call_with_retry()` calls
11. Rename test `test_llm_client_with_ureq_constructor` to `test_llm_client_with_reqwest_constructor`; update body

**Expected `otto check` result:** FAIL - errors in 4 files:
- `src/validator.rs` - `with_ureq` no longer exists; `call_with_retry` is async but called without `.await`
- `src/evaluator.rs` - same pattern
- `src/decomposer.rs` - `HttpClient::post` is async but called without `.await`
- `src/daemon/handlers/doc.rs` - `UreqClient` struct no longer exists; `LlmClient::with_ureq` no longer exists

---

### Step 2: `src/validator.rs` - Make DocValidator methods async

**Changes:**

1. In `DocValidator::new`: rename `LlmClient::with_ureq(config)` to `LlmClient::with_reqwest(config)` (constructor stays sync - constructing the client is not async)
2. Make `fn run_validation` -> `async fn run_validation`; add `.await` to `self.llm_client.call_with_retry(prompt)`
3. Make `fn validate_plan` -> `pub async fn validate_plan`; add `.await` to `self.run_validation(...)` call
4. Make `fn validate_spec` -> `pub async fn validate_spec`; add `.await`
5. Make `fn validate_phase` -> `pub async fn validate_phase`; add `.await`
6. Update doc comment on `new()` from "ureq" to "reqwest"
7. Tests: add `#[async_trait]` to `MockHttpClient`; make `fn post` -> `async fn post`
8. Tests: change `#[test] fn` -> `#[tokio::test] async fn`; add `.await` to `validator.validate_*()` calls

**Expected `otto check` result:** FAIL - errors in:
- `src/daemon/handlers/integrator.rs` - `validator.validate_plan(...)` etc. are now async but called without `.await`

---

### Step 3: `src/evaluator.rs` - Make CoverageEvaluator methods async

**Changes:**

1. In `CoverageEvaluator::new`: rename `LlmClient::with_ureq(config)` to `LlmClient::with_reqwest(config)`
2. Make `fn run_evaluation` -> `async fn run_evaluation`; add `.await` to `self.llm_client.call_with_retry`
3. Make `fn evaluate_plan_specs` -> `pub async fn evaluate_plan_specs`; add `.await`
4. Make `fn evaluate_spec_phases` -> `pub async fn evaluate_spec_phases`; add `.await`
5. Make `fn evaluate_phase_works` -> `pub async fn evaluate_phase_works`; add `.await`
6. Update doc comments from "ureq" to "reqwest"
7. Tests: add `#[async_trait]` to `MockHttpClient`; make `fn post` -> `async fn post`
8. Tests: change `#[test] fn` -> `#[tokio::test] async fn`; add `.await` to `evaluator.evaluate_*()` calls

**Expected `otto check` result:** FAIL - errors in:
- `src/daemon/handlers/integrator.rs` - `evaluator.evaluate_*()` now async (joining Step 2 errors)
- `src/decomposer.rs` - still broken from Step 1 (not yet fixed)
- `src/daemon/handlers/doc.rs` - still broken from Step 1 (not yet fixed)

---

### Step 4: `src/decomposer.rs` - Make decomposer functions async

**Changes:**

1. Add `use async_trait::async_trait;` to imports
2. Make `fn call_llm_for_children_raw` -> `async fn call_llm_for_children_raw`; add `.await` to `http_client.post()`
3. Make `fn call_llm_for_children` -> `async fn call_llm_for_children`; add `.await` to `http_client.post()`
4. Make `fn call_llm_for_validation` -> `async fn call_llm_for_validation`; add `.await` to `http_client.post()`
5. Make `fn call_llm_for_ratification` -> `async fn call_llm_for_ratification`; add `.await` to `call_llm_for_children_raw()`
6. Make `fn decompose_into` -> `async fn decompose_into`; add `.await` to `call_llm_for_children()`, `call_llm_for_validation()` calls
7. Make `pub fn decompose` -> `pub async fn decompose`; add `.await` to `decompose_into()`
8. Make `pub fn decompose_hierarchy` -> `pub async fn decompose_hierarchy`; add `.await` to `decompose_into()`, `decompose()`, `ratify_hierarchy()` calls
9. Make `fn ratify_hierarchy` -> `async fn ratify_hierarchy`; add `.await` to `call_llm_for_ratification()`
10. Change `http_client: &dyn HttpClient` parameter to `http_client: &(dyn HttpClient + Sync)` if needed for async trait object safety
11. Tests: add `#[async_trait]` to `SequenceMockHttp` impl; make `fn post` -> `async fn post`
12. Tests: change `#[test] fn` -> `#[tokio::test] async fn`; add `.await` to `decompose()` and `decompose_into()` calls

**Expected `otto check` result:** FAIL - errors in:
- `src/daemon/handlers/doc.rs` - `decompose_hierarchy()` now async; `UreqClient` still missing (from Step 1)

---

### Step 5: `src/daemon/handlers/doc.rs` - Make doc handlers async

**Changes:**

1. Remove `use crate::validator::client::UreqClient;` import (UreqClient no longer exists)
2. Add `use crate::validator::client::ReqwestClient;` import
3. In `accept_plan_markdown` line 195: replace `let client = UreqClient;` with `let client = ReqwestClient::new();`
4. In `classify_brief` line 469: replace `LlmClient::with_ureq(tier_config)` with `LlmClient::with_reqwest(tier_config)`
5. Make `fn classify_brief` -> `async fn classify_brief`; add `.await` to `client.call(&prompt)`
6. Make `fn accept_plan_markdown` -> `pub(super) async fn accept_plan_markdown`; add `.await` to `classify_brief()` and `decompose_hierarchy()` calls
7. Make `fn handle_doc_accept` -> `pub(super) async fn handle_doc_accept`; replace `try_handler!` with `try_async_handler!` (see Step 5a below)
8. Make `fn handle_doc_inject` -> `pub(super) async fn handle_doc_inject`; replace `try_handler!` with `try_async_handler!`
9. Remove the `tokio::runtime::Handle::try_current()` guard in `accept_plan_markdown` (lines 258-273) - all callers are now async, runtime is always available. Make the coordinator start unconditional:
   ```rust
   let start_req = DaemonRequest::new(0, "agent.start", json!({ "agent_type": "coordinator" }));
   let start_resp = super::dispatch(stores, event_tx, worktree_mgr, integrator_config, start_req);
   ```
   Note: `dispatch` is still sync at this point, so this call doesn't need `.await` yet
10. Tests: change `#[test] fn` -> `#[tokio::test] async fn`; add `.await` to `handle_doc_accept()` and `handle_doc_inject()` calls

**IMPORTANT:** The `try_handler!` macro in `daemon/handlers.rs:50-58` wraps the body in a sync closure `(|| -> eyre::Result<DaemonResponse> { $body })()`. This cannot wrap an async body. The four async handlers use `try_async_handler!` instead (added in Step 5a). All other handlers that remain sync keep using `try_handler!` unchanged.

**Step 5a: Add `try_async_handler!` macro to `src/daemon/handlers.rs`**

Add this macro immediately after the existing `try_handler!` macro (after line 59):
```rust
/// Async variant of try_handler! for handlers that contain .await calls.
macro_rules! try_async_handler {
    ($req_id:expr, $body:expr) => {{
        let __result: eyre::Result<DaemonResponse> = async { $body }.await;
        match __result {
            Ok(resp) => resp,
            Err(e) => DaemonResponse::err($req_id, RpcError::internal(&e.to_string())),
        }
    }};
}
```
This is a sub-step of Step 5 - add the macro before converting the doc handlers. No separate `otto check` needed; the macro is unused until the doc handlers reference it.

**Expected `otto check` result:** FAIL - errors in:
- `src/daemon/handlers.rs` - `handle_doc_accept` and `handle_doc_inject` now return futures but `dispatch` calls them as sync

---

### Step 6: `src/daemon/handlers/integrator.rs` - Make validator/coverage handlers async

**Changes:**

1. Make `fn handle_validator_validate` -> `pub(super) async fn handle_validator_validate`; replace `try_handler!` with `try_async_handler!`; add `.await` to `validator.validate_plan(...)`, `validator.validate_spec(...)`, `validator.validate_phase(...)` calls
2. Make `fn handle_coverage_evaluate` -> `pub(super) async fn handle_coverage_evaluate`; replace `try_handler!` with `try_async_handler!`; add `.await` to `evaluator.evaluate_*()` calls
3. Tests that call these handlers: change to `#[tokio::test] async fn`; add `.await`

**Expected `otto check` result:** FAIL - errors in:
- `src/daemon/handlers.rs` - `handle_validator_validate` and `handle_coverage_evaluate` now return futures but `dispatch` calls them as sync

---

### Step 7: `src/daemon/handlers.rs` - Make dispatch async

**Changes:**

1. Make `pub fn dispatch` -> `pub async fn dispatch`
2. Add `.await` to these 4 routes in the match:
   - `"doc.accept" => handle_doc_accept(...).await`
   - `"doc.inject" => handle_doc_inject(...).await`
   - `"validator.validate" => handle_validator_validate(...).await`
   - `"coverage.evaluate" => handle_coverage_evaluate(...).await`
3. All other routes remain sync calls (no `.await`) - they return `DaemonResponse` directly
4. Make `fn auto_start_agents` -> `async fn auto_start_agents`; add `.await` to the two `dispatch()` calls inside it; update `dispatch(...)` calls to `dispatch(...).await`
5. Update `auto_start_agents` call site in `dispatch`: add `.await`
6. Tests: change `#[test] fn` -> `#[tokio::test] async fn`; add `.await` to `dispatch()` calls

**IMPORTANT:** `dispatch` currently returns `DaemonResponse` synchronously. After this change it returns `impl Future<Output = DaemonResponse>`. The sync match arms still work because they return `DaemonResponse` directly, which is a valid final expression in an async fn.

**Expected `otto check` result:** FAIL - errors in:
- `src/ipc/server.rs` - `handler(req)` returns `DaemonResponse` but `handler` is `Fn(req) -> DaemonResponse` - the signature is wrong now because `dispatch` is async
- `src/daemon.rs` - closure `move |req| { handlers::dispatch(...) }` no longer returns `DaemonResponse`; it returns a future

---

### Step 8: `src/ipc/server.rs` - Make handle_client accept async handler

**Changes:**

1. Add `use futures::future::BoxFuture;` to imports
2. Change `handle_client` signature:
   ```rust
   // Before
   pub async fn handle_client(
       stream: UnixStream,
       handler: impl Fn(DaemonRequest) -> DaemonResponse + Send + 'static,
       ...
   )

   // After
   pub async fn handle_client(
       stream: UnixStream,
       handler: impl Fn(DaemonRequest) -> BoxFuture<'static, DaemonResponse> + Send + 'static,
       ...
   )
   ```
3. Change `handler(req)` call to `handler(req).await`
4. Tests: update all handler closures from `|req| DaemonResponse::ok(...)` to `|req| Box::pin(async move { DaemonResponse::ok(...) }) as BoxFuture<'static, DaemonResponse>`

**Expected `otto check` result:** FAIL - errors in:
- `src/daemon.rs` - closure type mismatch: closure returns future from `dispatch` but needs `BoxFuture<'static, ...>`

---

### Step 9: `src/daemon.rs` - Update handler closure to Box::pin async

**Changes:**

1. Update the handler closure (around line 510-515):
   ```rust
   // Before
   move |req| {
       handlers::dispatch(&stores, &handler_event_tx, &worktree_mgr, &integrator_config, req)
   }

   // After
   move |req: DaemonRequest| -> BoxFuture<'static, DaemonResponse> {
       let stores = stores.clone();
       let handler_event_tx = handler_event_tx.clone();
       let worktree_mgr = worktree_mgr.clone();
       let integrator_config = integrator_config.clone();
       Box::pin(async move {
           handlers::dispatch(&stores, &handler_event_tx, &worktree_mgr, &integrator_config, req).await
       })
   }
   ```
2. Add `use futures::future::BoxFuture;` to imports
3. The outer closure captures the Arcs; the inner async block clones them into its own scope for the `'static` lifetime bound

**Expected `otto check` result:** PASS

---

### Step 10: Remove ureq from Cargo.toml

**Command:** `cargo remove ureq`

**Expected `otto check` result:** PASS

This is the final step. The codebase now has zero references to `ureq`.

---

## File Change Summary

| Step | File | Nature of Change |
|------|------|-----------------|
| 1 | `src/validator/client.rs` | Replace UreqClient with ReqwestClient; make trait + LlmClient async |
| 2 | `src/validator.rs` | Make DocValidator methods async |
| 3 | `src/evaluator.rs` | Make CoverageEvaluator methods async |
| 4 | `src/decomposer.rs` | Make all LLM call + decompose functions async |
| 5 | `src/daemon/handlers/doc.rs` | Make doc handlers async; replace UreqClient usage |
| 6 | `src/daemon/handlers/integrator.rs` | Make validator/coverage handlers async |
| 7 | `src/daemon/handlers.rs` | Make dispatch + auto_start_agents async |
| 8 | `src/ipc/server.rs` | Change handle_client handler to BoxFuture |
| 9 | `src/daemon.rs` | Wrap handler closure in Box::pin(async move) |
| 10 | `Cargo.toml` | `cargo remove ureq` |

Total: **10 files** changed across **10 steps**.

## Execution Rules

These are not suggestions. They are requirements:

1. **Execute steps in order.** Step N fixes the errors introduced by Step N-1.
2. **Run `otto check` after completing each step.** Read every error.
3. **Do not start Step N+1 until Step N compiles** (expected downstream errors are OK - they are what Step N+1 fixes).
4. **No multi-file edits in a single step.** Each step touches exactly one file (plus its tests which are in the same file).
5. **Do not refactor, rename, or "improve" anything not listed in the step.** The scope is the async migration, nothing else.
6. **If `otto check` shows unexpected errors, STOP and diagnose before continuing.** The expected errors are listed in each step - anything else indicates a mistake.
7. **Commit after each step passes its expected state** (compiles or has only the expected downstream errors).

## Alternatives Considered

### Alternative 1: Parallel async trait alongside sync trait

- **Description:** Create `AsyncHttpClient` trait alongside `HttpClient`, create `AsyncLlmClient` alongside `LlmClient`, migrate consumers one by one.
- **Pros:** Every intermediate state compiles cleanly.
- **Cons:** Doubles the trait/client surface area temporarily. More code to write, more to delete later. The temporary duplication is confusing.
- **Why not chosen:** The bottom-up cascade approach is cleaner and the expected errors at each step are predictable and mechanical. The duplication approach adds more total lines changed and more risk of wiring mistakes.

### Alternative 2: `spawn_blocking` for ureq calls

- **Description:** Keep ureq, wrap calls in `tokio::task::spawn_blocking`.
- **Pros:** No signature cascade.
- **Cons:** Still blocks a thread (in the blocking pool). Leaves ureq as a dependency alongside reqwest. Does not solve the fundamental problem.
- **Why not chosen:** Moves the problem, doesn't fix it.

### Alternative 3: `tokio::task::block_in_place`

- **Description:** Wrap ureq calls with `block_in_place`.
- **Pros:** No dependency changes.
- **Cons:** Requires multi-thread runtime (breaks some test setups). Still holds a thread. Fragile.
- **Why not chosen:** Fragile and leaves ureq in place.

## Technical Considerations

### Dependencies

- `reqwest` - already at v0.13.2 with `json` and `stream` features. No change needed.
- `async-trait` - already at v0.1.89. No change needed.
- `ureq` - REMOVED in Step 10.

No new dependencies.

### Performance

Decomposition calls remain sequential (parent before children). Async releases the Tokio thread between awaits rather than blocking it. No parallelization of LLM calls.

### Security

No change.

### Testing Strategy

All existing test assertions remain valid. Mechanical changes only:
- Mock `HttpClient` impls get `#[async_trait]` and `async fn post`
- Test functions calling async methods get `#[tokio::test] async fn`
- Handler test calls get `.await`
- IPC server test closures wrap in `Box::pin(async move { ... })`

### The try_handler! / try_async_handler! Macros

The `try_handler!` macro (`daemon/handlers.rs:50-58`) wraps a sync closure:
```rust
let __result = (|| -> eyre::Result<DaemonResponse> { $body })();
```

This cannot wrap an async body. A companion `try_async_handler!` macro is added (Step 5a) that wraps an async block instead:
```rust
let __result: eyre::Result<DaemonResponse> = async { $body }.await;
```

**Four handlers** switch from `try_handler!` to `try_async_handler!`:
- `handle_doc_accept`
- `handle_doc_inject`
- `handle_validator_validate`
- `handle_coverage_evaluate`

All other handlers remain sync and keep using `try_handler!` unchanged. If this macro causes compilation issues (e.g. lifetime or borrow problems with the async block), fall back to explicit `match` in those four handlers and delete `try_async_handler!`.

### The Handle::try_current() Guard

`accept_plan_markdown` (doc.rs:258) has a guard:
```rust
if tokio::runtime::Handle::try_current().is_ok() { ... }
```

This existed because sync tests had no Tokio runtime. After migration, all tests are `#[tokio::test]` and all callers are async. The guard is removed; the coordinator start becomes unconditional.

**However:** at Step 5, `dispatch` is still sync. The recursive `super::dispatch()` call for coordinator start does not need `.await` yet. At Step 7 when dispatch becomes async, this call gains `.await`.

### ReqwestClient Construction

The `ReqwestClient` struct holds a `reqwest::Client` with a configured timeout:

```rust
use std::time::Duration;

pub struct ReqwestClient {
    client: reqwest::Client,
}

impl ReqwestClient {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("reqwest client construction is infallible with valid config");
        Self { client }
    }
}
```

This replaces `UreqClient` which had no configuration. The 120s timeout matches the LLM call expectations documented in the codebase.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `async_trait` desugaring conflicts with clippy or Rust 2024 | Low | Med | Already in use in the codebase; tested with current toolchain |
| `accept_plan_markdown` recursive dispatch call (coordinator start) needs careful `.await` timing | Med | High | Step 5 leaves it sync; Step 7 adds `.await` when dispatch becomes async |
| Test closures for IPC server handler need `'static` lifetime via cloning | Med | Med | Explicitly documented in Step 8 and Step 9 |
| `double_write_old_records` uses `std::fs::read_to_string` inside an async fn | Low | Low | Files are small .md docs; no need for `tokio::fs` |
| Decomposer `&dyn HttpClient` may need `+ Sync` bound for async dispatch | Low | Low | `async_trait` handles this automatically for `Box<dyn HttpClient>` |

## Open Questions

None. All questions from the prior attempt have been resolved through implementation analysis.

## References

- `src/validator/client.rs` - HttpClient trait, UreqClient, LlmClient (lines 1-365)
- `src/validator.rs` - DocValidator (lines 1-357)
- `src/evaluator.rs` - CoverageEvaluator (lines 1-448)
- `src/decomposer.rs` - decompose_hierarchy and LLM call functions (lines 1-942)
- `src/daemon/handlers/doc.rs` - doc.accept/inject handlers, classify_brief (lines 1-678)
- `src/daemon/handlers/integrator.rs` - validator.validate, coverage.evaluate handlers (lines 265-459)
- `src/daemon/handlers.rs` - dispatch function, try_handler! macro (lines 50-207)
- `src/ipc/server.rs` - handle_client function (lines 62-113)
- `src/daemon.rs` - handler closure (lines 504-517)
- `src/daemon/context.rs` - Stores.validator, Stores.evaluator fields (lines 64-66)
