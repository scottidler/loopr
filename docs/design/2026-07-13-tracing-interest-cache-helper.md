# Design Document: Kill the log-capture flake class workspace-wide

**Author:** Scott Idler (drafted by Claude)
**Date:** 2026-07-13
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Two crates (llm, store) carry identical private copies of `ensure_global_interested_default()`, the fix for the tracing interest-cache log-capture flake. Two more unit-test binaries (loopr, worktree) contain the same unprotected pattern and are latent flakes. Promote the helper into `telemetry`, route every `set_default`/`with_default` test site in the workspace through it (or exempt with a cited comment), delete the per-crate copies.

## Problem Statement

### Background

- tracing's process-global per-callsite `Interest` cache is computed lazily on first callsite hit. With <=1 registered dispatcher, tracing-core computes interest via the registering thread's thread-local default.
- Failure mode: in a multi-test binary, a sibling test with no subscriber first-hits a shared `warn!`/`error!` callsite -> `Interest::never()` is cached process-wide -> a later log-capture test's buffer is empty.
- Thread-local `set_default` cannot fix this. The fix is a process-global always-interested discarding subscriber installed once per test binary (`InterestedDiscard` + `Once`).
- Shipped per-crate in commit b1b076ed: llm (`crates/llm/src/logcapture.rs:66-70`, baseline ~8% fail over 200 runs) and store (`crates/store/src/logcapture.rs:69-75`, ~16% over 150). Post-fix: 0/300 each.

### Problem

The fix exists as two identical private copies, and the class is NOT dead:

- **loopr unit binary (295 tests), HIGH risk, unprotected:** 6 bare `set_default(json_subscriber(...))` sites in `crates/loopr/src/daemon/context/transition/tests.rs` (:188, :229, :266, :307, :347, :377) assert exact WARN/ERROR counts. The asserted callsites (`crates/loopr/src/daemon/context/transition.rs:346`, `:364`) are also exercised subscriber-less by sibling tests in handler/tests.rs, daemon/context/tests.rs, daemon/tests.rs. Textbook poisoning setup; just not caught yet.
- **worktree unit binary (60 tests), HIGH risk, unprotected:** `crates/worktree/src/tests.rs:127, :153` (`with_default`) assert `count_at_level(ERROR) == 1` on the `error!` at `crates/worktree/src/lib.rs:107`. `LOG_LOCK` (tests.rs:30) serializes only the two capturing tests against each other; `delete_branch_rejects_non_loopr_branch` (tests.rs:13) and `delete_branch_rejects_plain_feature_branch` (tests.rs:171) hit the same callsite unserialized with no subscriber.
- MEDIUM: `init_for_test` callers (loopr/src/daemon/tests.rs:80, :99; daemon/context/tests.rs:248; the `*_visibility.rs` integration binaries across tools, agents, integrator, worktree, loopr; telemetry/tests/events_log_contract.rs). Closed wholesale by having `init_for_test` call the helper.
- LOW: 7 `crates/*/tests/instrumentation.rs` binaries (mostly single-test, capture-first) and telemetry's own src/tests.rs sites (:374 etc., emitting callsites live inside the test file).

Two copies of the same helper is exactly the "recurring cross-page inconsistency demands shared code that kills the class" case. Every future crate that adds a log-capture test re-derives or re-copies the fix, or flakes.

### Goals

- One canonical `ensure_global_interested_default()` in `telemetry`, zero per-crate copies.
- Every `set_default`/`with_default` test site in the workspace routes through the shared helper or carries a one-line exemption comment citing why it is safe.
- The two unprotected HIGH-risk binaries (loopr, worktree) are proven flake-free with the same looped-run evidence standard used in b1b076ed.
- `telemetry::init_for_test` calls the helper internally so all visibility-test binaries are covered with zero call-site churn.

### Non-Goals

- Expanding e2e target coverage (multi-Work DAGs, review-reject cycles, integration conflicts). Separate effort, separate doc; parked until this ships.
- Wiring the remint-collision `warn!` to a metric. Deferred in the ID-collision doc; stays deferred ("make it be a problem first").
- Changing production `telemetry::init` behavior. Test-support only.
- Fixing any flake class other than tracing interest-cache poisoning (the domain id-collision birthday-paradox flake was fixed separately in b757c247).

## Proposed Solution

### Overview

Move store's copy (the fuller doc comment) into `telemetry` as a plain `pub fn` in a `testing` module. Route all sites through it. Delete both per-crate copies. Prove with looped runs.

### Architecture

- New module `crates/telemetry/src/testing.rs`:
  - `InterestedDiscard`: 20-line `Subscriber` impl, `enabled() -> true`, all else no-ops. std-only (fits telemetry's "must compile without tokio" rule).
  - `pub fn ensure_global_interested_default()`: `static Once` + `set_global_default(InterestedDiscard)`, result ignored, matching the shipped copies. The ignored result is vestigial by construction: the Phase 0 inventory guarantees no helper-routed binary contains another global-default installer, so the race cannot be lost to an incompatible subscriber (an `EnvFilter`-backed global CAN cache `Interest::never`; such a binary gets exempted, never routed).
- Exported as plain `pub fn`, NOT behind a cargo feature. Defined in `telemetry::testing`, re-exported at the crate root beside `init_for_test`; call sites use `telemetry::ensure_global_interested_default()`. In-crate precedent: `init_for_test` + `TestSubscriberGuard` (subscriber.rs:210, :275) already ship un-gated in telemetry's normal surface. Copy the proven in-house pattern (see Resolved Decisions).
- `init_for_test` calls the helper first -> every `init_for_test` caller is covered with zero call-site churn: the loopr daemon unit tests (the MEDIUM class above) and all the `*_visibility.rs` / `events_log_contract.rs` integration binaries.
- Dependency edge: store gains `telemetry` as a **dev-dependency** (store has no telemetry dep today; no cycle, telemetry depends on no workspace crate). store/CLAUDE.md Dependencies section updated. All other affected crates already depend on telemetry normally.

### Interaction analysis (why the global default is safe)

- `set_default`/`with_default` override the global on the capturing thread; capture subscribers see everything they saw before. `InterestedDiscard` only receives events from subscriber-less threads and discards them.
- With >=2 registered dispatchers, per-callsite interests combine via `Interest::and` (tracing-core 0.1.36 callsite.rs:496): differing interests yield `sometimes`, NOT `always`. The property that matters survives either way: with the always-interested helper registered, a cached `never` cannot stick, `enabled()` gets re-consulted, and capture subscribers see their events. Per-layer `EnvFilter`s still filter at the Layer level. (Do not "optimize" against a union-interest model; that model is false.)
- Empirical: b1b076ed shipped this exact configuration into llm and store binaries (mixed capturing + non-capturing tests), 0/300 each.
- One real hazard: any in-process global-subscriber installer conflicts with the helper. That is production `telemetry::init` via `.try_init()` (subscriber.rs:370, fails AlreadyInitialized if the helper won the race) AND the bare `SubscriberInitExt` spelling: `tracing_subscriber::registry().with(layer).init()`. One such caller exists TODAY: `crates/llm/tests/span.rs:104` (import at :23), in its own integration binary. That binary is exempted, never routed. Phase 0 inventories the full installer set, including the `.init()` spelling an obvious grep misses.

### Data Model

None. No records, no schema.

### API Design

```rust
// crates/telemetry/src/testing.rs; re-exported at crate root.
// Call sites: telemetry::ensure_global_interested_default()
/// Install a process-global always-interested discarding subscriber, once.
/// Call before `tracing::subscriber::set_default` in any log-capture test.
/// (full interest-cache mechanism doc comment migrated from store)
pub fn ensure_global_interested_default();
```

`init_for_test` signature unchanged; it gains an internal call to the helper.

### Implementation Plan

#### Phase 0: Pre-change evidence baseline + installer inventory
**Model:** sonnet
- MUST run before Phase 1 lands. loopr's unit binary calls `init_for_test` (daemon/tests.rs:80, :99; daemon/context/tests.rs:248); once `init_for_test` installs the helper, a same-binary "baseline" is contaminated and a null result proves nothing.
- 200-run baseline loops of `cargo test -p loopr --lib` and `cargo test -p worktree --lib` at pre-Phase-1 HEAD; record fail counts. Null result accepted and recorded.
- Inventory every in-process global-subscriber installer across test code: `telemetry::init`, `.try_init(`, `set_global_default`, AND the `SubscriberInitExt` spellings (`.init()` on a subscriber builder). Seed the exemption list; `crates/llm/tests/span.rs:104` is already known.
- **Success criteria:** baseline numbers and the installer inventory recorded in this doc's Evidence addendum.

#### Phase 1: Promote the helper into telemetry
**Model:** sonnet
- New module `crates/telemetry/src/testing.rs` (`mod testing` + re-export in lib.rs beside `init_for_test`); migrate store's doc comment, the fuller of the two.
- `init_for_test` calls it before installing its own subscriber.
- Extend telemetry/CLAUDE.md Visibility-contract section with the interest-cache contract.
- **Success criteria:** fn exported from telemetry; `init_for_test` calls it first; telemetry per-crate `otto ci` green.

#### Phase 2: Route llm + store, delete the copies
**Model:** sonnet
- llm/metered.rs and store's `set_capturing_default` helpers call the telemetry fn; delete both `logcapture.rs` files and their `mod` decls.
- Add `telemetry` dev-dep to store/Cargo.toml; update store/CLAUDE.md.
- **Success criteria:** `rg "mod logcapture" crates/` -> 0 hits; both crates' `otto ci` green; 300 looped runs each at 0 failures (`for i in $(seq 300); do cargo test -p <crate> --lib -q || { echo "FAIL at run $i"; break; }; done` prints no FAIL).

#### Phase 3: Fix the two unprotected HIGH-risk binaries
**Model:** opus
- Before/after evidence: Phase 0 already holds the pre-change baseline; this phase supplies the post-fix 300-run side. Both land in the Evidence addendum.
- loopr: local `set_capturing_default` helper in transition/tests.rs calling the telemetry fn; route all 6 sites.
- worktree: call the helper before both `with_default` sites. `LOG_LOCK` stays (it serializes buffer assertions, a different job).
- Break-to-prove: remove the `error!` at worktree/src/lib.rs:107, confirm the ERROR==1 test fails, restore.
- **Success criteria:** 300 looped runs of each unit binary (`cargo test -p loopr --lib`, `cargo test -p worktree --lib`), 0 log-capture failures; break-to-prove documented at the call site per b1b076ed convention.

#### Phase 4: Total sweep of remaining sites
**Model:** sonnet
- Re-verify the Phase 0 installer inventory (including the `.init()`/`SubscriberInitExt` spellings) against current HEAD; any binary with an in-process installer gets the exemption path, never the helper.
- Route telemetry's own src/tests.rs sites through the helper (same crate, trivial) unless caught by the inventory.
- 7 instrumentation.rs binaries + any other `set_default`/`with_default` site: route through helper, or leave the fixed-shape exemption comment: `// interest-cache exempt: <reason> (see telemetry::testing)`.
- Audit is assert-style, not a judgment call. The audit command (exits non-zero on any unclassified file):

  ```
  rg -l 'set_default|with_default' crates/ | while read -r f; do
    rg -q 'ensure_global_interested_default|interest-cache exempt' "$f" \
      || { echo "UNCLASSIFIED: $f"; exit 1; }
  done
  ```

- **Success criteria:** audit command exits 0; workspace `otto ci` green.

#### Phase 5: Docs true-up
**Model:** sonnet
- Root CLAUDE.md crate map note for store's new dev-dep edge.
- Flip this doc's status to Implemented.
- **Success criteria:** docs match shipped reality; workspace `otto ci` green.

Phase 0 is an evidence baseline, not an environmental spike: the mechanism + fix efficacy are already proven in-repo at 0/300 x 2 crates; what Phase 0 protects is the integrity of the before/after comparison and the installer exemption list.

## Acceptance Criteria

- [ ] `rg "ensure_global_interested_default" crates/` shows exactly one definition, in `crates/telemetry/`.
- [ ] `crates/llm/src/logcapture.rs` and `crates/store/src/logcapture.rs` do not exist.
- [ ] The Phase 4 audit command exits 0: every `set_default`/`with_default` file under `crates/` references the helper or carries the `interest-cache exempt` comment.
- [ ] 300 looped runs each of `cargo test -p loopr --lib` and `cargo test -p worktree --lib` complete with 0 log-capture failures.
- [ ] The Evidence addendum in this doc holds the Phase 0 baseline numbers, the installer inventory, and the post-fix 300-run numbers.
- [ ] Workspace `otto ci` green; store/CLAUDE.md and telemetry/CLAUDE.md reflect the new edge and contract.

## Resolved Decisions

- **2026-07-13, plain pub fn over `test-support` cargo feature.** telemetry already exports `init_for_test`/`TestSubscriberGuard` un-gated; copy the in-house pattern. A feature flag adds a knob with no consumer asking for it.
- **2026-07-13, no committed loop script.** The 300-run validation is a one-time evidence gate, not recurring process; the exact command lives in this doc's success criteria. bin/ stays e2e-only.
- **2026-07-13, bounded baseline attempt, null result accepted.** Try to reproduce the latent loopr/worktree flake pre-fix (200 runs); do not block on reproduction, the mechanism is established.
- **2026-07-13, total sweep.** Every site routes or is exempted with a comment; no silent "low risk, skipped" sites. Siblings behave identically.
- **2026-07-13 (panel), baseline moved to Phase 0.** Staff Engineer, verified: loopr's unit binary has `init_for_test` callers, so any post-Phase-1 baseline is contaminated. Baseline runs at pre-Phase-1 HEAD or it is worthless.
- **2026-07-13 (panel), installer inventory includes the `.init()` spelling and runs as a Phase 0 precondition.** Staff Engineer, verified: `crates/llm/tests/span.rs:104` installs a global default via `SubscriberInitExt::init()`, which the naive grep set missed. That binary is exempted.
- **2026-07-13 (panel), public path pinned.** Defined in `telemetry::testing`, re-exported at crate root; call sites use `telemetry::ensure_global_interested_default()`. Symmetry with `init_for_test`.
- **2026-07-13 (panel), assert-style audit.** Fixed exemption-comment shape + a documented audit command that exits non-zero on unclassified files, replacing the eyeball check.
- **2026-07-13 (panel), evidence lands in this doc.** Baseline, inventory, and 300-run numbers go in the Evidence addendum, not terminal scrollback.

## Alternatives Considered

### Alternative 1: `test-support` cargo feature on telemetry
- **Description:** Gate the helper behind `features = ["test-support"]`; crates add it in dev-dependencies.
- **Pros:** Test-only API stays out of production builds.
- **Cons:** Breaks symmetry with `init_for_test` (already un-gated); every consumer carries feature plumbing; the helper is 25 lines of inert std code in prod builds either way.
- **Why not chosen:** In-house precedent wins; the "cost" being avoided is negligible.

### Alternative 2: Status quo (per-crate copies)
- **Description:** Copy `logcapture.rs` into loopr and worktree too.
- **Pros:** No new dependency edges.
- **Cons:** Four copies of the same 70 lines; every future log-capture test re-copies or flakes; doc comments drift (llm's and store's already differ).
- **Why not chosen:** This is the exact class-vs-spot-fix call; the class fix is cheap and telemetry is the natural home.

### Alternative 3: Dedicated `test-support` crate
- **Description:** New workspace crate holding test utilities.
- **Pros:** Clean separation of test-only code.
- **Cons:** A new crate for one function; telemetry is already the declared owner of "tracing subscriber composition" per its CLAUDE.md.
- **Why not chosen:** Wrong blast-radius trade; telemetry's scope already covers it.

## Technical Considerations

### Dependencies
- New workspace edge: store -> telemetry (dev-dependency only). No cycle; no new external crates.

### Performance
- Test binaries only, zero production impact. `enabled() -> true` disables the zero-cost `never` fast path, so `trace!`/`debug!` on subscriber-less test threads construct an Event before discard. Level-filtering `InterestedDiscard` to cut this is a deferred follow-up (see Addendum); the shipped, proven helper is unfiltered and we copy it.

### Security
- None. Test-only code path; no data handled.

### Testing Strategy
- Looped full-binary runs (the b1b076ed standard): Phase 0 pre-change baseline, 300 post-fix runs at 0 failures, both recorded in the Evidence addendum.
- Break-to-prove on the worktree ERROR==1 assertion.
- otto ci per phase (per-crate during phases, workspace at the end).

### Rollout Plan
- Single repo, no cross-repo blast radius. Phases land in order, one commit each, each otto-ci-green. No deployment surface; ships with the next workspace version bump.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Global default interferes with an existing capture assertion | Low | Med | Interaction analysis above + 0/300 empirical evidence in llm/store; Phase 4 sweep re-runs every affected binary under otto ci |
| A test binary mixes the helper with an in-process global installer (`try_init`, `SubscriberInitExt::init()`) -> AlreadyInitialized / conflicting defaults | Low | Low | Phase 0 inventory covers all spellings; installer binaries are exempted; telemetry/CLAUDE.md documents the contract |
| Baseline reproduction burns time without reproducing | Med | Low | Bounded at 200 runs; null result recorded, not blocking |
| Sweep misses a site added mid-flight | Low | Med | Acceptance criterion is a fresh `rg` at the end, not the Phase-0 inventory |

## Open Questions

(none)

## Addendum

### Evidence (filled during implementation)

- **Phase 0 baseline (pre-Phase-1 HEAD `b757c247`, sandbox-disabled so sccache works): loopr --lib 0/200 failures, worktree --lib 14/200 raw failures (~7%).** The worktree raw count was NOT output-classified (discarded to /dev/null); Phase 3's captured/classified 400-run pre-fix probe later showed it is tracing-flake (~7.8%) plus a ~1% unrelated seq-race. See Phase 3 for the clean split. loopr is a null result (accepted per Resolved Decisions) — but the class is real there too: a flake was caught live on a calibration run of `cargo test -p loopr --lib` (exit 101, then a clean 294-pass), and the mechanism is already proven at 0/300 x2 in b1b076ed. loopr's low frequency is consistent with the doc's "latent, just not caught yet."
  - Note: the loop MUST run sandbox-disabled. Under the command sandbox, `RUSTC_WRAPPER=sccache` fails `Operation not permitted` on any (re)compile, turning every recompiling iteration into a spurious `could not compile` "failure" (an early botched run reported loopr 200/200 for exactly this reason). Mixing sandboxed and unsandboxed cargo invocations churns the build fingerprint and forces those recompiles.
- **Phase 0 installer inventory (at `b757c247`):** the only in-process global-default subscriber installer in *test* code is `crates/llm/tests/span.rs:104` (`tracing_subscriber::registry().with(layer).init()`, `SubscriberInitExt` import at :23), in its own integration binary. **Exemption list: `crates/llm/tests/span.rs` only.** Everything else is thread-local:
  - `telemetry::init_for_test` installs via **thread-local `set_default`** (`crates/telemetry/src/subscriber.rs:255`), not global -> all `init_for_test` callers (loopr daemon unit tests; every `*_visibility.rs` integration binary; `telemetry/tests/events_log_contract.rs:35`) are covered by routing the helper *inside* `init_for_test`, zero call-site churn.
  - `.try_init()` (`crates/telemetry/src/subscriber.rs:370`) is inside **production `telemetry::init` only** — not reachable from any test.
  - All `registry().with(layer)` sites used by capture tests (`transition/tests.rs:166`, `worktree/src/tests.rs:60`, `store/src/bundles/tests.rs:283`, `store/src/works/tests.rs:53`, `llm/src/metered.rs:527`, `telemetry/src/tests.rs:372/419/537`) return a subscriber consumed by **thread-local `set_default`/`with_default`** — routable, not global installers.
- **Phase 2 post-fix (after routing llm + store through `telemetry::ensure_global_interested_default`, copies deleted): llm --lib 0/300, store --lib 0/300.** Re-confirms the b1b076ed result holds through the refactor.
- **Phase 3 post-fix: loopr --lib 0/300 (0 failures of any kind). worktree --lib 0/300 log-capture failures** (the raw run showed 3/300, all the unrelated seq-race below — 0 tracing-assertion failures).
- **Phase 3 clean classified before/after for worktree (400 runs each, sandbox-disabled, failure output captured and classified):**
  - **Pre-fix (helper reverted): 35/400 = 31 tracing-flake + 4 seq-race.** The 31 are the `delete_branch_guard_refusal_logs_error` / `delete_branch_tolerated_git_failure_does_not_log_error` ERROR-count assertions (both fail together in a poisoned run — shared `error!` callsite). ~7.8% tracing flake — this is the real worktree flake rate.
  - **Post-fix: 0/400 tracing-flake + 2/400 seq-race.** The tracing flake is eliminated; the seq-race is unaffected (as expected — separate class).
  - Supersedes the raw Phase 0 "worktree 14/200": that count was tracing + seq-race mixed (output was discarded). This classified run is the honest measurement.
- **Separate finding (OUT OF SCOPE for this doc):** `worktree::handle::tests::concurrent_creates_for_same_work_id_all_get_distinct_seqs` flakes ~1% independently of the tracing fix — a 10-thread race in `Worktree::create` seq allocation where a create occasionally returns an error the `SeqTaken` retry loop doesn't absorb. Pre-existing, unrelated to the tracing interest-cache class. Filed as a follow-up; not addressed here.

### Rejected / deferred (recorded so they are not relitigated)

- **Level-filtered `InterestedDiscard` (Architect).** Deferred, not gating: cut the Event-construction cost on subscriber-less test threads by filtering the helper to `<= WARN`. Rejected for now because the shipped b1b076ed helper is unfiltered and proven at 0/300 x 2; copy the proven pattern. Revisit only if test-binary runtime becomes an observed problem.
- **Rollback section (Architect).** Rejected: test-only utility code, no deployment surface; rollback is `git revert` of phase commits. Generic template completeness, no content.
- **`test-support` cargo feature.** Rejected in initial review round; see Alternatives Considered.
- **Committed loop script (`bin/flake-loop`).** Rejected: one-time evidence gate, not recurring process; the exact commands live in the phase success criteria. Both reviewers endorsed.

## References

- Commit b1b076ed: fix(tests): kill log-capture flake via global interested tracing default (llm + store, evidence numbers)
- Commit b757c247: the separate domain id-collision flake fix (out of scope here)
- `crates/store/src/logcapture.rs` doc comment: full mechanism writeup (to be migrated)
- `crates/telemetry/CLAUDE.md`: crate scope ("owns tracing subscriber composition", "must compile without tokio")
- Memory: loopr-v5 recovery-chain follow-ups (item 1: this flake class)
