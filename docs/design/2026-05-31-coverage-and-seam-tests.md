# Design Document: Coverage task + seam-test gap analysis

**Author:** Scott A. Idler
**Date:** 2026-05-31
**Status:** Implemented (2026-05-31)
**Review Passes Completed:** 5/5
**Crates touched:** workspace `.otto.yml` (cov task, landed); analysis only for decomposer / worktree / loopr

## Summary

Adds a one-time coverage signal (`otto cov`) and uses it to answer "where are the real seam-test gaps?" The original proposal here was to add `tests/`-level seam tests to `decomposer` and `worktree`. **The coverage run plus an Architect review killed that proposal:** both crates are already ~97% line-covered and their in-crate tests already cross their seams with real public types, so dedicated seam tests would be redundant. The real seam-test gap is in the `loopr` daemon's recovery/dispatch paths, where the workspace's ~1,550 uncovered lines actually live.

## Problem Statement

### Background

v5 mandates seam testing (repo `CLAUDE.md` rule #1: every crate boundary gets a round-trip serde test and an integration test that crosses the seam with real types). An audit (2026-05-31) found the discipline is honored well on the high-traffic boundaries (`agents` `seam_*`, `integrator` `integrate_seam`, `store`, `ipc` wire round-trips, and the `loopr` `stage_8`/`stage_9` pipeline tests). Two crates (`decomposer`, `worktree`) had no `tests/`-level crossing test, only in-crate `src/.../tests.rs` and `instrumentation.rs`. That surface gap is what this doc set out to close.

### What the coverage run found

`otto cov` (2026-05-31, workspace):

| Scope | Lines | Functions |
|---|---|---|
| Workspace | 92.0% (18488/20086) | 90.6% |
| decomposer | 97% (907/929) | - |
| worktree | 97% (648/668) | - |

Lowest files: `worktree/src/lib.rs` 81% (9/11), `worktree/src/ops.rs` 84%, `worktree/src/handle.rs` 89%; every `decomposer` source file is >=92%.

### The actual problem (reframed)

The surface gap (no `tests/` file) is **not** a real gap:

1. **decomposer is already seam-tested in-crate.** `crates/decomposer/src/decompose/tests.rs` calls `decompose(&plan, target, &llm)` through its public signature with real public inputs (`domain::Plan`, `llm::ScriptedLlm` via the `stub` feature, real `context::PromptLoader`, real `telemetry::transcript` path) and already asserts deps/AC/cycles and transcript writing (`happy_path_two_children_with_one_dep`, `transcript_written_on_success`, `transcript_written_on_cycle_detected`, ...). A `tests/` file would duplicate this. The compiler already pins the structural contract: if `decompose`'s signature changes, the `loopr` daemon fails to build.

2. **worktree is nearly the same story.** `crates/worktree/src/handle/tests.rs` exercises `Worktree::create` against a real git repo, and `loopr/src/daemon/startup/tests.rs:77` creates a real `Worktree` end-to-end. The only thin spot is that the `lib.rs` facade free-functions (`list`, `parse_branch`, `cleanup_at`, `delete_branch`) are not exercised *together* in one lifecycle. That is a marginal, optional gap, not a doc-worthy one.

3. **The real uncovered code is elsewhere.** decomposer + worktree account for <50 of the ~1,550 uncovered workspace lines. The rest live in the `loopr` daemon (reconcile / dispatch / recovery, ~120 source files) and the `agents`/`integrator` I/O boundaries, where an untested behavioral regression actually crashes the daemon rather than failing a build.

### Goals

- A `cov` otto task for one-time / ad-hoc coverage signal. **(Landed.)**
- Record the eval outcome so nobody re-proposes the redundant decomposer/worktree seam tests.
- Point the next seam-test effort at the `loopr` daemon recovery/dispatch paths (separate follow-up doc).

### Non-Goals

- A coverage-percentage CI gate (`cov` stays out of `ci`).
- Adding decomposer/worktree `tests/` seam tests (rejected, see Decision).
- Implementing the loopr daemon seam tests here (that earns its own doc once the target paths are identified).
- The integration-branch -> target-`main` promotion question (separate, roadmap Stage 9 caveat).

## What landed: the `cov` task

`.otto.yml` gained `cov` (`cargo llvm-cov --workspace --all-features --json`, writes HTML + JSON, emits totals as otto outputs) and `cov-report` (consumes those outputs; `--fail-under`, `--json`, `--details` params). Adapted from the `ottofile` skill's `bash/rust-cov.sh` + `bash/cov-report.sh` to the `--workspace` variant. **Not** wired into `ci` (one-time signal, not a gate). This is the part of the doc that delivered value: it produced the numbers that killed the rest of the proposal.

## Decision: do NOT add decomposer/worktree seam tests

After the Architect review (full record below), the decomposer seam test is rejected as redundant and the worktree lifecycle test is downgraded to "optional, marginal." Neither is implemented. The structural contract is compiler-enforced; the behavioral contract is already covered in-crate with real public types.

If we ever want the one genuinely-missing worktree assertion (the `lib.rs` facade lifecycle in a single round-trip), it is a ~20-line addition to `crates/worktree/src/handle/tests.rs` (or a small `tests/lifecycle.rs`), using the real struct name **`Worktree`** (not `WorktreeHandle`, which does not exist) and the real signature `Worktree::create(repo_path, worktree_root, work_id, sha)`. Low priority.

## The actual work: targeted loopr seam tests

Round-2 consensus with the Architect named two files (`daemon/startup.rs`, `transport/handler.rs`) as the seam-reachable, fork-free subset of loopr's 702 uncovered lines (~350 reachable; the rest is `double_fork`/PID-lock/signal/XDG mechanics no in-process test reaches). Reading the existing tests shrinks the net-new surface further - much is already covered:

- `startup.rs`: worktree sweep is covered (in-crate `startup/tests.rs`), director respawn is covered (`tests/director_reconcile.rs`), in-progress-Work recovery is covered (`tests/director_stuck_states.rs`). **Not covered:** the startup-reconcile re-drive of `sweep_bundles` (stuck non-terminal Bundles on cold boot) and `sweep_dep_promotions` (promote newly-unblocked Works on cold boot).
- `handler.rs`: handshake/status/plan_create/record_list/director_chat/plan_override/director_status are covered (`handler/tests.rs`, 14 tests). **Not covered:** `handle_record_get` and the error-variant arms of `map_store_error`.

This is a small, surgical addition, not a campaign. The honest target is the handful of confirmed-uncovered behavioral branches above.

### Implementation Plan

#### Phase 1: handler.rs dispatch seam gaps
**Model:** sonnet
- Read `coverage.json` (`~/.otto/loopr-v5-998cab1b/.../tasks/cov/coverage.json`) for the exact uncovered lines in `crates/loopr/src/transport/handler.rs`; read `handler/tests.rs` first to avoid duplication.
- Add tests in `crates/loopr/src/transport/handler/tests.rs` for: `handle_record_get` (happy path returns the record; unknown id yields NotFound; missing/invalid kind yields the right RpcError), and each currently-uncovered arm of `map_store_error`.
- Reuse the existing `stub_ctx` / `dummy_anthropic` / `init_git_repo` helpers. Deterministic, no fork, no network, no barrier.
- Verify: `cargo test -p loopr --lib transport::handler`.

#### Phase 2: startup.rs cold-boot re-drive seam
**Model:** sonnet
- Read the uncovered lines in `crates/loopr/src/daemon/startup.rs` (`sweep_bundles`, `sweep_dep_promotions` startup paths); read `startup/tests.rs` + `tests/director_stuck_states.rs` first to avoid duplication.
- Add tests (in `startup/tests.rs`) that seed a Store with a stuck non-terminal Bundle and a Work whose dependency just reached Done, build a `DaemonContext`, call `reconcile()`, and assert `sweep_bundles` re-drives the bundle and `sweep_dep_promotions` promotes the unblocked Work. Use the existing `setup`/`seed_repo` helpers.
- Deterministic; no daemon fork (call `reconcile()` directly on a constructed context, the way `startup/tests.rs` already does).
- Verify: `cargo test -p loopr --lib daemon::startup`.

#### Phase 3: confirm + close
**Model:** sonnet
- `otto ci` at workspace root (green).
- `otto cov` once; confirm `handler.rs` and `startup.rs` line% moved up (these tests, unlike the rejected decomposer/worktree ones, should actually move the needle). Record the delta in this doc.
- If any targeted branch turns out to be a fork/PID/signal path after all, note it as structurally-untestable and skip it (do not contort a test to reach a forking path).

### Honest caveat on the contract-vocabulary idea

A CLI-verb / IPC-method vocabulary guard was discussed as the class that "would have caught" the `bin/e2e` `plan`->`plan create` break. It would NOT have: that break was a *bash script* calling a stale binary invocation, which no Rust test sees. A Rust contract test only catches Rust-client<->daemon desync. The bash-to-binary contract is only caught by an integration test that invokes the real CLI (which the `stage_*` tests already do for the happy path). Deferred; not in this plan.

## Alternatives Considered

### Alternative 1: Write the decomposer + worktree seam tests anyway (the original proposal)
- **Why not chosen:** 97%/97% already; in-crate tests cross the seams with real public types; the compiler pins structure. Redundant, and it adds a second test suite to keep in sync with the LLM tool-call schema.

### Alternative 2: Coverage-percentage CI gate
- **Why not chosen:** vanity metric for a clean-boundary design; punishes legitimately-untestable code. Keep `cov` ad-hoc.

### Alternative 3: Redirect to loopr daemon seam tests (chosen direction)
- **Why chosen:** that is where the uncovered lines and the actual crash-risk live.

## Technical Considerations

### Testing Strategy
- Whatever loopr daemon seam tests follow must be deterministic: no `tokio::sync::Barrier`, no timing (the 7-hour 2026-05-24 hang was a barrier-gated seam test that deadlocked when one branch returned early; see `docs/design/2026-05-25-seam-reviewer-concurrency-rewrite.md`). Manufacture conditions in the inputs.
- Coverage baseline recorded above; re-ran `otto cov` after the loopr seam tests to confirm they move the needle (unlike the decomposer/worktree tests, which would not have). **Result:** `transport/handler.rs` 79% -> **94%** (34 -> 10 uncovered lines), `daemon/startup.rs` 71% -> **82%** (37 -> 23 uncovered), workspace 92.0% -> 92.1%. The remaining uncovered lines in both files are the fork/PID/signal paths the consensus already ruled structurally untestable in-process.

## Architect Review (2026-05-31)

Reviewed by the Gemini Architect persona (Design Review mode). Verdict: the original two-seam-test proposal is not worth implementing. Findings, all accepted:

1. **Contract-pinning justification is weak.** Rust's compiler already enforces the structural contract (a `pub` signature change breaks the `loopr` build); the in-crate tests already enforce the behavioral contract using the same public inputs a `tests/` file would use. The "consumer view" distinction does not buy a new class of caught regression here.
2. **decomposer test is fully redundant** with `decompose/tests.rs` (including the transcript-path assertions, which already exist exhaustively).
3. **Factual error caught:** the original doc referenced `WorktreeHandle::create`; the struct is `Worktree` (`crates/worktree/src/handle.rs:27`), so the proposed test would not have compiled. Corrected above.
4. **worktree test has only marginal value** (the `lib.rs` facade lifecycle is not exercised together), not doc-worthy on its own.
5. **Effort is misallocated:** the ~1,550 uncovered workspace lines live in the `loopr` daemon and `agents`/`integrator` I/O paths, not in these two 97% crates. Redirect there.

The Architect's hardest question - "what runtime regression are these tests actually preventing rather than satisfying a checklist?" - had no good answer, which is why the proposal was withdrawn.

**Round 2 (consensus).** Claude pushed back on three points with per-crate coverage data the Architect had inferred rather than measured: (1) finding #5's redirection target was imprecise (integrator only 29 lines; tools/telemetry/derive missed entirely); (2) "loopr's uncovered lines" is not "daemon recovery seams" - ~half are CLI/plumbing or fork-coupled and not seam-reachable; (3) rule #1 is not bureaucratic for cross-process boundaries (the `bin/e2e` break proves the class). The Architect conceded all three, verified `commands/director.rs`/`lib.rs` are CLI/pre-fork scaffolding and `daemon.rs`/`session.rs` are fork/PID/signal mechanics, and converged with Claude on `startup.rs` + `transport/handler.rs` as the seam-reachable target. The Implementation Plan above reflects that consensus.

## Open Questions
- [x] Which specific `loopr` functions are lowest-covered and seam-reachable? **Resolved (round-2 consensus):** `transport/handler.rs` (`handle_record_get`, `map_store_error` arms) and `daemon/startup.rs` (`sweep_bundles`/`sweep_dep_promotions` cold-boot re-drive). See Implementation Plan.
- [ ] Is the marginal `worktree` facade-lifecycle assertion worth ~20 lines, or left alone? (Deferred; low priority.)
- [ ] Is a CLI/IPC contract-vocabulary guard worth a small harness? (Deferred; would not have caught the `bin/e2e` bash break - see caveat above.)

## References
- repo `CLAUDE.md` working rule #1 (seam tests)
- `docs/design/2026-05-25-seam-reviewer-concurrency-rewrite.md` (determinism lesson)
- `ottofile` skill: `bash/rust-cov.sh`, `bash/cov-report.sh`
- coverage baseline: `~/.otto/loopr-v5-998cab1b/.../tasks/cov/coverage.json`
- `crates/worktree/src/handle.rs:27` (`pub struct Worktree`), `loopr/src/daemon/startup/tests.rs:77` (real `Worktree::create` call site)
