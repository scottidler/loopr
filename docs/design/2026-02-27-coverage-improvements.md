# Design Document: Loopr v3 — Test Coverage Improvements

**Author:** Scott Aidler + Claude
**Date:** 2026-02-27
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Loopr v3 has 91.9% line coverage (21,450/23,346) across 1,092 tests — strong for a 35k-line codebase. However, five files sit well below the project average, and the lack of a `lib.rs` makes `main.rs` (0% coverage) structurally untestable. This document targets the six highest-ROI improvements: extract a library crate, then close coverage gaps in the five most impactful files — `agents/executor.rs` (73.4%), `tui/run.rs` (75.3%), `domain/proposal.rs` (77.1%), `domain/decision.rs` (78.0%), and `agents/llm_client.rs` (81.6%).

## Problem Statement

### Background

Coverage was measured via `cargo llvm-cov` (now wired as `otto cov`). The project has excellent coverage in domain models (99%+), handlers (92.6%), IPC (93-96%), and config (96.3%). But six files drag the average down, and the binary-only crate structure prevents testing `main.rs` at all.

### Problem

1. **No `lib.rs`** — All modules are declared in `main.rs`. The binary crate cannot be imported by integration tests or benchmarks. Functions like `ensure_daemon()` and `setup_logging()` are completely untestable from outside. This is a structural problem, not a test-writing problem.

2. **`agents/executor.rs` at 73.4%** — The core agent action dispatch. 324 uncovered lines. 9 of 17 action handlers have zero test coverage (Commit, ProposeBundle, CreateLearning, all four Create* record actions, SpawnResearcher, ValidateDocument). The auto-restart logic and worktree cleanup paths are also untested.

3. **`tui/run.rs` at 75.3%** — 141 uncovered lines. Terminal I/O initialization, event loop edge cases, `dispatch_ipc_action()` error paths, and `refresh_collection()` deserialization failures.

4. **`domain/proposal.rs` at 77.1% and `domain/decision.rs` at 78.0%** — Scaffold stubs from audit defect #22. The `Record` trait implementations (`id()`, `updated_at()`, `indexed_fields()`) have zero test coverage.

5. **`agents/llm_client.rs` at 81.6%** — 53 uncovered lines. HTTP error handling (rate limits, non-2xx status codes), SSE stream buffer edge cases (incomplete UTF-8, malformed JSON), and broadcast channel failure paths.

### Goals

- Extract `lib.rs` so all modules are importable and `main.rs` becomes a thin entrypoint
- Raise `agents/executor.rs` from 73.4% to 90%+
- Raise `tui/run.rs` from 75.3% to 85%+
- Raise `domain/proposal.rs` and `domain/decision.rs` to 95%+
- Raise `agents/llm_client.rs` from 81.6% to 90%+
- Raise overall project coverage from 91.9% to 93%+

### Non-Goals

- 100% coverage on any file — diminishing returns on terminal I/O, signal handlers, and OS-level operations
- Testing `main.rs` beyond what the `lib.rs` extraction enables — the remaining entrypoint code (arg parsing, tokio::main) is inherently integration-level
- Refactoring production code beyond the `lib.rs` extraction — this is a testing effort, not a refactoring effort
- Adding coverage gates to CI — that's a follow-up decision after seeing the improved numbers

## Proposed Solution

### Overview

Six changes, ordered by dependency and ROI:

1. **Phase 0: Extract `lib.rs`** — Move all `mod` declarations from `main.rs` to `src/lib.rs`. Make `main.rs` a thin wrapper that calls library functions. This unblocks testability for `ensure_daemon()` and `setup_logging()`, and follows standard Rust crate conventions.

2. **Phase 1: Close trivial gaps** — Add `Record` trait tests to `proposal.rs` and `decision.rs`. ~6 lines of test code for ~22 lines of coverage.

3. **Phase 2: Cover executor action handlers** — Add tests for the 9 untested action handlers in `executor.rs`, plus error paths for the auto-restart logic and worktree cleanup.

4. **Phase 3: Cover LLM client error paths** — Add tests for HTTP error handling, SSE buffer edge cases, and broadcast failure paths in `llm_client.rs`.

5. **Phase 4: Cover TUI run paths** — Add tests for deserialization failures in `refresh_collection()`, IPC dispatch error paths, and draw edge cases in `tui/run.rs`.

### Phase 0: Extract `lib.rs`

Move all module declarations from `main.rs` to a new `src/lib.rs`:

```rust
// src/lib.rs
pub mod agents;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod domain;
pub mod error;
pub mod id;
pub mod ipc;
pub mod tools;
pub mod tui;
pub mod validator;
pub mod worktree;
```

Move `setup_logging()` to `lib.rs` as a public function. Move `ensure_daemon()` to `daemon/mod.rs` as a public function — it's daemon lifecycle logic (PID file checks, process spawning, socket readiness) and belongs with the daemon module. `main.rs` becomes:

```rust
// src/main.rs
use clap::Parser;
use eyre::{Context, Result};
use log::info;

use loopr::cli::{self, Cli, Command};
use loopr::config::Config;
use loopr::daemon;
use loopr::domain;
use loopr::tui;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    loopr::setup_logging().context("Failed to setup logging")?;

    let cli_args = Cli::parse();
    let config = Config::load(cli_args.config.as_ref())
        .context("Failed to load configuration")?;
    let role = cli_args.r#as.unwrap_or(domain::role::Role::Coordinator);

    match cli_args.command {
        Some(Command::Daemon) => {
            info!("Starting daemon");
            let (ctx, _event_tx) = daemon::context::DaemonContext::shared(config)?;
            daemon::daemon_main(ctx).await.context("daemon exited with error")
        }
        Some(Command::Tui) | None => {
            daemon::ensure_daemon(&config.daemon.pid_path, &config.daemon.socket_path)?;
            info!("Starting TUI, connecting to daemon at {}",
                config.daemon.socket_path.display());
            tui::run::run_tui(&config.daemon.socket_path).await
                .context("TUI failed")?;
            Ok(())
        }
        Some(ref cmd) => {
            daemon::ensure_daemon(&config.daemon.pid_path, &config.daemon.socket_path)?;
            info!("CLI command: {:?}", cmd);
            cli::dispatch::run(cmd, &config.daemon.socket_path, role).await
        }
    }
}
```

Move `integration_tests` from `#[cfg(test)] mod integration_tests` in `main.rs` to `src/lib.rs` (same `#[cfg(test)]` pattern). Integration tests use `crate::` imports which remain valid since they're now inside the library crate.

**Impact:** `main.rs` drops from 108 lines to ~35 lines. The 0% coverage becomes irrelevant (tiny entrypoint). `ensure_daemon()` becomes testable as `loopr::daemon::ensure_daemon()`. `setup_logging()` becomes testable as `loopr::setup_logging()`.

**Visibility adjustments:** Modules that are currently private (`mod agents`) become `pub mod agents`. This is fine — the library crate is internal to the project, not published to crates.io. All `use crate::` paths inside existing modules remain unchanged — only `main.rs` switches to `use loopr::`.

**Prerequisite check:** `cli::Cli` and `cli::Command` must be `pub` for `main.rs` to import them. Verify current visibility before implementing — they likely already are since they derive `clap::Parser`.

### Phase 1: Close Trivial Gaps (`proposal.rs`, `decision.rs`)

Add `Record` trait method tests to both files:

**`domain/proposal.rs`** — add to existing `#[cfg(test)] mod tests`:
```rust
#[test]
fn test_proposal_record_id() {
    let p = Proposal::new("Title".into(), "Desc".into(), "author-1".into());
    assert_eq!(Record::id(&p), &p.id);
}

#[test]
fn test_proposal_record_updated_at() {
    let p = Proposal::new("Title".into(), "Desc".into(), "author-1".into());
    assert_eq!(Record::updated_at(&p), p.updated_at);
}

#[test]
fn test_proposal_record_indexed_fields() {
    let p = Proposal::new("Title".into(), "Desc".into(), "author-1".into());
    let fields = Record::indexed_fields(&p);
    assert!(fields.contains_key("status"));
}
```

**`domain/decision.rs`** — identical pattern:
```rust
#[test]
fn test_decision_record_id() {
    let d = Decision::new("Title".into(), "Rationale".into(), "decider-1".into());
    assert_eq!(Record::id(&d), &d.id);
}

#[test]
fn test_decision_record_updated_at() {
    let d = Decision::new("Title".into(), "Rationale".into(), "decider-1".into());
    assert_eq!(Record::updated_at(&d), d.updated_at);
}

#[test]
fn test_decision_record_indexed_fields() {
    let d = Decision::new("Title".into(), "Rationale".into(), "decider-1".into());
    let fields = Record::indexed_fields(&d);
    assert!(fields.contains_key("status"));
}
```

**Expected impact:** Both files jump from ~78% to ~95%. Total effort: ~18 lines of test code.

### Phase 2: Cover Executor Action Handlers (`executor.rs`)

Target: 73.4% → 90%+. Nine untested action handlers plus error/lifecycle paths.

**Group A — Record creation actions (quick wins, identical pattern):**

Each Create* action is a bridge wrapper: build JSON params, call `bridge.request()`, extract `id` from response. Test each with success and error cases using the existing `test_stores()` + `dispatch()` pattern from `integration_tests.rs`.

| Action | Test | Approach |
|--------|------|----------|
| `CreatePlan` | `test_execute_create_plan` | Already tested in integration_tests; add unit test in executor for error path |
| `CreateSpec` | `test_execute_create_spec` | Same pattern |
| `CreatePhase` | `test_execute_create_phase` | Same pattern |
| `CreateWork` | `test_execute_create_work` | Same pattern |

**Group B — Git operations (medium complexity):**

| Action | Test | Approach |
|--------|------|----------|
| `Commit` | `test_execute_commit_success`, `test_execute_commit_git_add_failure` | Create tmpdir git repo, stage files, verify commit. Error: non-existent path |
| `ProposeBundle` | `test_execute_propose_bundle` | Create tmpdir git repo with branch, verify bridge call params |

**Group C — Agent management:**

| Action | Test | Approach |
|--------|------|----------|
| `SpawnResearcher` | `test_execute_spawn_researcher` | Verify bridge call includes `agent_type: "researcher"`, `query`, `target_id` |
| `CreateLearning` | `test_execute_create_learning_with_all_fields`, `test_execute_create_learning_minimal` | Test with and without optional fields (confidence, tags, work_id) |
| `ValidateDocument` | `test_execute_validate_document_pass`, `test_execute_validate_document_fail` | Mock bridge response with pass/fail verdicts |

**Group D — Lifecycle paths:**

| Path | Test | Approach |
|------|------|----------|
| Auto-restart | `test_coordinator_auto_restart_on_failure` | Mock agent loop to fail twice then succeed; verify 3 attempts made |
| Auto-restart cancellation | `test_coordinator_restart_cancelled_during_sleep` | Set session to Cancelled during restart delay |
| Worktree cleanup success | `test_worktree_cleanup_after_implementer` | Run Implementer task, verify worktree dir removed |
| Worktree cleanup skip | `test_no_worktree_cleanup_for_thinking_plane` | Run Coordinator task, verify no cleanup attempted |
| Failed agent terminal state | `test_failed_agent_sets_error_message` | Mock agent loop to return Err, verify session.error_message is set |

**Expected impact:** +150-200 lines of test code covering ~250 of the 324 uncovered lines. Target: 90%+.

### Phase 3: Cover LLM Client Error Paths (`llm_client.rs`)

Target: 81.6% → 90%+. Focus on error injection.

| Gap | Test | Approach |
|-----|------|----------|
| HTTP 429 rate limit | `test_call_streaming_rate_limit` | Mock server returning 429; verify error contains "rate limit" |
| HTTP 500 server error | `test_call_streaming_server_error` | Mock server returning 500; verify error propagated |
| Network error | `test_call_streaming_network_error` | Use invalid URL; verify connection error |
| Malformed SSE JSON | `test_parse_sse_malformed_json` | Feed `data: {broken` through parser; verify None returned |
| Incomplete UTF-8 in stream | `test_read_sse_stream_incomplete_utf8` | Feed raw bytes with split multi-byte char; verify recovery |
| Broadcast channel dropped | `test_emit_chunk_no_subscribers` | Create client with no receivers; verify no panic |
| Empty response body | `test_call_streaming_empty_response` | Mock server returning 200 with empty body; verify graceful handling |

**Expected impact:** +50-70 lines of test code covering ~40 of the 53 uncovered lines. Target: 90%+.

### Phase 4: Cover TUI Run Paths (`tui/run.rs`)

Target: 75.3% → 85%+. Focus on testable paths, skip terminal I/O.

| Gap | Test | Approach |
|-----|------|----------|
| `refresh_collection()` bad JSON | `test_refresh_collection_invalid_json` | Feed malformed JSON for each collection type; verify no panic, state unchanged |
| `dispatch_ipc_action()` error | `test_dispatch_ipc_action_send_failure` | Mock IpcClient that returns Err on send; verify warn logged |
| `apply_daemon_event()` unknown event | `test_apply_daemon_event_unknown_type` | Send event with unknown type; verify noop |
| Draw with zero-size terminal | `test_draw_zero_size_terminal` | Create 0x0 terminal backend; verify no panic |
| `refresh_collection()` for all 8 types | `test_refresh_collection_all_types` | Verify each collection type (plans, specs, phases, works, bundles, ticks, learnings, locks) handles valid data |

**Expected impact:** +40-60 lines of test code. Target: 85%+ (terminal I/O setup paths remain uncovered by design).

## Alternatives Considered

### Alternative 1: Skip `lib.rs` extraction, focus only on test additions
- **Description:** Add tests without restructuring the crate
- **Pros:** Less disruption, no import path changes
- **Cons:** `main.rs` stays at 0%, `ensure_daemon()` and `setup_logging()` remain untestable, doesn't follow Rust conventions
- **Why not chosen:** The `lib.rs` extraction is low-risk, improves the project structure, and is a prerequisite for proper testability. It's also standard Rust practice for binaries with substantial logic.

### Alternative 2: Use `#[cfg(test)]` test binaries instead of `lib.rs`
- **Description:** Keep binary-only crate, test via `cargo test --bin loopr`
- **Pros:** No structural change
- **Cons:** Can't test `pub(crate)` functions from external test files, can't use the crate as a library dependency, integration tests remain awkward
- **Why not chosen:** `lib.rs` is strictly better — same testability plus library reuse.

### Alternative 3: Target all files below 90% instead of top 5
- **Description:** Also add tests for `daemon/mod.rs` (82.6%), `worktree/manager.rs` (86.7%), `agents/integrator_task.rs` (86.6%)
- **Pros:** More comprehensive
- **Cons:** `daemon/mod.rs` coverage gaps are signal handling and process management — HARD to test with low ROI. `worktree/manager.rs` and `integrator_task.rs` are already above 85% with mostly error-path gaps.
- **Why not chosen:** Diminishing returns. The top 5 files deliver the most coverage improvement per line of test code. The remaining files can be addressed in a follow-up if needed.

### Alternative 4: Use `cargo-mutants` for mutation testing instead of line coverage
- **Description:** Mutation testing finds code that's "tested" but where tests don't actually catch bugs
- **Pros:** Higher-quality signal than line coverage
- **Cons:** Very slow on a 35k-line codebase, noisy results, doesn't address the structural `lib.rs` problem
- **Why not chosen:** Line coverage gaps are the immediate problem. Mutation testing is a good follow-up after coverage reaches 93%+.

## Technical Considerations

### Dependencies

- No new external crate dependencies
- Phase 0 (`lib.rs` extraction) changes import paths throughout the codebase — the `use crate::` paths remain unchanged within `lib.rs`, but `main.rs` switches to `use loopr::`
- Phases 1-4 add test code only — zero production code changes

### Performance

- No runtime performance impact (test code only)
- `otto cov` will take slightly longer as test count grows (~50-80 new tests)
- Coverage instrumentation already adds ~2x compile time; this is unchanged

### Testing Strategy

Each phase is independently testable:
- Phase 0: `otto ci` passes (compile + clippy + fmt + all existing tests)
- Phase 1: `otto cov` shows proposal.rs/decision.rs at 95%+
- Phase 2: `otto cov` shows executor.rs at 90%+
- Phase 3: `otto cov` shows llm_client.rs at 90%+
- Phase 4: `otto cov` shows tui/run.rs at 85%+

### Rollout Plan

Each phase is a separate commit. `otto ci` after each. `otto cov` after Phase 4 to measure final numbers.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `lib.rs` extraction breaks import paths | Low | Low | All `use crate::` paths inside modules are unchanged. Only `main.rs` imports change (mechanical). `otto ci` catches any breakage immediately. |
| New tests are flaky (tmpdir git repos, async timing) | Low | Medium | Use unique tmpdir per test (existing pattern). Avoid real network calls. Use `tokio::test` with controlled timing. |
| Coverage numbers don't improve as expected | Low | Low | Measure per-phase. Adjust test targets if specific paths prove untestable. |
| Test code bloat outweighs coverage benefit | Low | Low | Each phase targets highest-ROI gaps. Stop at diminishing returns. |

## Open Questions

- [x] Should `ensure_daemon()` move to `lib.rs` root or to `daemon/mod.rs`? **Resolved:** `daemon/mod.rs`. It's daemon lifecycle logic (PID file checks, process spawning, socket readiness). Keeping it with the daemon module is semantically correct and makes it testable alongside the other daemon lifecycle code.
- [ ] Should we set a coverage threshold in CI (e.g., `otto cov --fail-under 90`)? Deferred — measure first after all phases complete, gate later.

## Files to Modify

| Phase | File | Change |
|-------|------|--------|
| 0 | `src/lib.rs` (new) | Module declarations + `setup_logging()` + `#[cfg(test)] mod integration_tests` |
| 0 | `src/main.rs` | Strip to thin entrypoint (~35 lines) |
| 0 | `src/daemon/mod.rs` | Add `pub fn ensure_daemon()` (moved from main.rs) |
| 1 | `src/domain/proposal.rs` | Add 3 Record trait tests |
| 1 | `src/domain/decision.rs` | Add 3 Record trait tests |
| 2 | `src/agents/executor.rs` | Add ~15 tests for action handlers + lifecycle |
| 3 | `src/agents/llm_client.rs` | Add ~7 tests for error paths |
| 4 | `src/tui/run.rs` | Add ~5 tests for deserialization + error paths |

## Implementation Order

1. Phase 0: `lib.rs` extraction → `otto ci`
2. Phase 1: proposal.rs + decision.rs Record tests → `otto ci`
3. Phase 2: executor.rs action handler tests → `otto ci`
4. Phase 3: llm_client.rs error path tests → `otto ci`
5. Phase 4: tui/run.rs testable path tests → `otto ci`
6. Final: `otto cov --details` → measure and report

## References

- Coverage baseline: `otto cov --details` (2026-02-27) — 91.9% lines, 92.6% functions
- Design doc: `docs/design/2026-02-26-multi-level-rwl.md` (source of truth)
- Audit fixes: `docs/design/2026-02-27-audit-fixes.md`
- E2E blockers: `docs/design/2026-02-27-e2e-blockers.md`
