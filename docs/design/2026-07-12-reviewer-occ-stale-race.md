# Design Document: Reviewer OCC Self-Stale Race Fix

**Author:** Scott Idler
**Date:** 2026-07-12
**Status:** Approved; Phase 1 implemented (see Addendum: Implementation status)
**Review Passes Completed:** 5/5

## Summary

Every bundle wedges at Triaged: the reviewer persists its Review, then loses its own
`Triaged -> Reviewed` OCC write, and reconcile re-spawns it into a deterministic ~34s
doom loop. Root cause is the daemon discarding the floored `updated_at` that
`BundlesStore::update` returns, so the in-memory Bundle stales itself. Fix: a
`transition_and_persist_bundle` helper (the pattern the other record kinds already
have), plus a loud-fail branch in the arm that currently swallows the Stale.

## Problem Statement

### Background

- verified-swarm (docs/design/2026-07-11) landed code phases 1-19. Phase 0/20 live
  e2e validation is the remaining gate.
- Live repro (bin/e2e rust-version, 2026-07-12, daemon at c104a1ba): reviewer runs,
  persists a Review row, bundle stays Triaged, reviewer exits, reconcile WARNs
  `Triaged Bundle with no live Reviewer; re-spawning`. 4 Review rows for bundle
  `bd-cx1u1`, 0 ticks, integrator never reached. Stopped manually after ~2min of
  LLM spend in the loop.
- The implementation notes (2026-07-11-verified-swarm-implementation-notes.md:18)
  flagged 3 red `failure_paths` tests with a suspected shared root cause. Confirmed:
  same mechanism (reproduced `reject_then_recover_reaches_tick` red locally; the
  `complete_free called with empty queue` panic is doomed re-spawn rounds draining
  the ScriptedLlm FIFO).

### Problem

The reviewer's OCC write loses to a stale timestamp the daemon itself created. Not
an external writer race. Mechanism:

1. `BundlesStore::update` floors the persisted timestamp
   (`floored = max(now_millis(), current.updated_at + 1)`) and **returns** it so
   callers re-sync (`crates/store/src/bundles.rs:223-226`). Commit `71746cdc`
   threaded the returned value through the integrator, `transition_and_persist_work`,
   `transition_and_persist_plan`, and SummaryFanout. It missed the reviewer triage
   site and `accept_bundle`.
2. `spawn_reviewer_for_bundle` step 1 (`crates/loopr/src/daemon/context.rs:940-953`):
   snapshots `expected = bundle.updated_at`, transitions to Triaged, persists via
   `if let Err(e) = self.store.bundles().update(...)`. The `Ok(floored)` is
   discarded. Local copy and disk now diverge (live log: off by exactly 1ms, the
   floor's `current + 1` signature when propose and triage land in the same ms).
3. `run_reviewer` snapshots its OCC token from the stale local copy
   (`crates/agents/src/reviewer.rs:325`); its final `Triaged -> Reviewed/Rejected`
   write (`reviewer.rs:338-340`) fails `StoreError::Stale`.
4. The caller's F6 arm (the `ReviewerError::Update(Stale)` match arm on the
   reviewer task result; F6 is its label from the code-review remediation) treats
   every Stale as "another reviewer won" and silently drops the verdict
   (`context.rs:1128-1131`). Bundle stays Triaged.
5. Reconcile re-spawns (`crates/agents/src/director.rs:633-645`). The re-spawn is
   deterministically doomed: the fresh reviewer's copy matches disk, but the triage
   step's `transition(Triaged, Triaged)` returns `Transition::Unchanged` (no local
   `updated_at` bump, `crates/domain/src/bundle.rs:181-187`) while the store write
   still executes unconditionally, flooring disk to now. Every round loses by
   construction, and the write resets reconcile's age clock (grace 30s + review
   4-9s = the observed ~34s period).

Live-log proof (`events.log`, bundle `bd-cx1u1`): round 1
`expected updated_at=1783899472958, actual=1783899472959` (the +1 floor); rounds 2-4
each `expected` = previous round's `actual`, each `actual` = the re-spawn triage
write's wall time. The interleaving writer is the triage step itself.

### Goals

- Reviewer verdicts land: `Triaged -> Reviewed/Rejected` succeeds on the first
  round with no stale conflict.
- The 3 red `failure_paths` tests go green; `stage_8` / `stage_9` stop flaking.
- Kill the class, not the site: no daemon call site hand-rolls
  snapshot/transition/update on Bundles again.
- A Stale write whose bundle never actually advanced (still-Triaged in the
  reviewer arm, still-Reviewed in the accept arm) can never again fail silent.

### Non-Goals

- Store-crate changes. The OCC contract (`crates/store/src/bundles.rs:175-227`) is
  correct; callers dropping its return value is the bug.
- The `run_logs_tail_on_empty_target_errors_no_runs_found` env-race flake from the
  implementation notes. Separate test-hygiene issue, unrelated mechanism.
- Multi-reviewer-per-bundle support. The live-set guard means at most one reviewer
  per bundle today; see Open Questions for the `Review.round` note.

## Proposed Solution

### Overview

Copy the proven in-house pattern. Bundles are the only record kind in the daemon
without a `transition_and_persist_*` helper; every hand-rolled site is where the
class recurs (`71746cdc` fixed 3 sites, missed 2). Add the helper, convert all raw
call sites in one commit, and harden the F6 arm so the silent-drop path only covers
the genuine concurrent-winner case.

### Architecture

**New helper** `transition_and_persist_bundle` in `crates/loopr/src/daemon/context.rs`,
mirroring `transition_and_persist_work` (`context.rs:1617-1623`):

- snapshot `expected = bundle.updated_at`
- `bundle.transition(target, actor)`
- on `Transition::Unchanged`: **skip the store write entirely** (this is the second
  half of the fix; the unconditional write is what dooms re-spawns)
- on `Transition::Changed`: `store.bundles().update(...)`, then
  `bundle.updated_at = returned_floored_value`
- takes the bundle sink trait (`&dyn BundleUpdateSink`, the same seam the reviewer
  writes through) rather than `&Store`, so a stub sink can count writes: the
  "zero disk writes on Unchanged" criterion is asserted at that seam
- errors are Bundle-specific: new `BundleTransitionError` with `Stale`/`Persist`
  variants and bundle-worded messages. NOT a reuse of `TransitionError`
  (`context.rs:1518-1524`), whose messages read "stale work" / "works().update";
  a log line that says "work" about a bundle is a names-tell-the-truth violation.
  Stale stays a distinct variant so callers keep exit-cleanly-on-lost-race.

Recovery story this buys: a reconcile re-spawned reviewer fresh-reads the bundle
(copy matches disk), its `Triaged -> Triaged` triage returns Unchanged, the skip
leaves disk untouched, and its final `Triaged -> Reviewed/Rejected` write carries a
matching token and wins. The doom loop cannot form.

One-reviewer-per-bundle is enforced by composition, not by any single guard:
single daemon (pid lock, `daemon.rs:513`), reconcile filtering spawn on the live
set (`director.rs:631`) plus the 30s grace, and startup requeue running before
accepts. `ScopedIdGuard` (`context.rs:99`) only tracks membership (insert on
spawn, remove on exit); it never rejects a duplicate. The other spawn sources are
direct handoff (`context.rs:839`) and startup requeue (`startup.rs:360`). Under
that composed invariant, proceeding past Unchanged is safe.

Deliberate consequence of Unchanged-skip: the daemon loses the ability to bump
`updated_at` without a status change (a "status-preserving touch"). Intentional.
No heartbeat exists today, and the unconditional touch resetting reconcile's age
clock is exactly the doom-loop mechanism being removed. If a touch is ever needed,
the seam is an explicit `force_touch` on the helper; not now.

**Converted call sites** (all raw `.bundles().update` in the daemon):

| site | today | after |
|---|---|---|
| triage, `context.rs:947` | discards `Ok(floored)` | helper; primary defect |
| supersede, `context.rs:1004` | discards `Ok(floored)` (benign: returns immediately) | helper; hygiene |
| accept, `spawner.rs:148` | discards, then hands stale bundle to `spawn_integrator_for_bundle` | helper; latent same-class bug (integrator prologue `Accepted -> Integrating` at `crates/integrator/src/lib.rs:746-768` uses `bundle.updated_at` as its token) |

**F6 hardening** (`context.rs:1128-1131`): on `ReviewerError::Update(Stale)`,
re-read the Bundle.

- Reached Reviewed/Rejected: keep silent-winner behavior (genuine race, someone won).
- Still Triaged: `error!` loudly. Invariant violation; there is no winner. No new
  Work transition: after the helper fix, reconcile's re-spawn recovers correctly
  (fresh read, Unchanged-skip), so this branch needs visibility, not routing. Costs
  one wasted review round in a case that should be unreachable.

### Data Model

No schema changes. `Review` rows stay append-only with `round = prior count + 1`
(`reviewer.rs:272-273`); the 4 duplicate rows in the repro are a symptom of the
loop, not a missing uniqueness guard.

### API Design

No public API changes. `BundleUpdateSink` signatures untouched. The reviewer
(`crates/agents/src/reviewer.rs:325, 338-340`) is correct as-is once fed a synced
timestamp.

### Implementation Plan

#### Phase 1: transition_and_persist_bundle helper + convert call sites
**Model:** sonnet
- Add the helper per the Architecture spec: `&dyn BundleUpdateSink` parameter,
  `BundleTransitionError`, Unchanged-skip, re-sync `updated_at` from the returned
  floored value.
- Convert triage (`context.rs:947`), supersede (`context.rs:1004`), accept
  (`spawner.rs:148`); these are the only three write sites (grep-verified
  2026-07-12). `accept_bundle` hands the refreshed bundle to
  `spawn_integrator_for_bundle`.
- Each site keeps its current error behavior (exit cleanly on Stale, warn/error
  and return on other failures); only the success path changes.
- **Success criteria:**
  - `rg -U '\.bundles\(\)\s*\.update' crates/loopr/src/daemon/` hits only the
    helper (multiline: the method call spans lines at every current site).
  - Unit test: helper-returned bundle's `updated_at` equals the on-disk value.
  - Unit test: an already-Triaged bundle produces zero writes in the triage step,
    asserted against a stub `BundleUpdateSink` that counts calls.

#### Phase 2: break-to-prove regression tests
**Model:** sonnet
- Store-seam test forcing the same-millisecond floor: create + immediately triage +
  review-transition on a synced copy succeeds; the pre-fix discard shape fails.
- Integration test driving `spawn_reviewer_for_bundle` end-to-end with a scripted
  verdict: Bundle lands Reviewed/Rejected with exactly 1 Review row.
- **Success criteria:**
  - The 3 `failure_paths` tests green; `stage_8_plan_to_tick` and
    `stage_9_director_plan_to_tick` green, 3 consecutive runs.
  - Each new test demonstrated red by temporarily restoring the discard shape
    (break-to-prove; the broken variant is shown, not kept in the suite).

#### Phase 3: loud-fail Stale discrimination (F6 arm + accept_bundle arm)
**Model:** opus
- F6 arm (`context.rs:1128-1131`): on Stale, re-read the bundle. Branches fully
  enumerated:
  - re-read fails -> `error!` (fail loud; include both the Stale and the re-read
    error)
  - Reviewed/Rejected or any later status (Accepted, Superseded, ...) -> forward
    progress happened; keep silent-winner (`debug!`)
  - still Triaged -> `error!` invariant violation, with bundle id, expected/actual
    timestamps, and round. `ReviewerError::Update(Stale)` carries only the
    timestamps (`reviewer.rs:72`); round is obtained by listing the bundle's
    Review rows at error time (extra read confined to the loud path).
- Same discrimination in `accept_bundle`'s Stale-swallow arm (`spawner.rs`):
  re-read fails -> `error!`; moved past Reviewed -> silent; still Reviewed ->
  `error!`. Siblings behave identically; kills the silent class in both arms.
- **Success criteria:**
  - Unit test per branch, both arms; the loud branches break-to-proven.

#### Phase 4: live e2e verification (operator step, not a phase agent)
**Model:** n/a (operator)
- `bin/e2e rust-version` with a real API key. Needs `ANTHROPIC_API_KEY`, absent in
  non-interactive shells per the implementation notes.
- **Success criteria:**
  - Run reaches the integrator; ticks > 0.
  - events.log: zero `stale record` errors on the reviewer span; zero
    `reconcile: Triaged Bundle with no live Reviewer` WARNs; zero Phase 3
    invariant-violation ERRORs.
  - Exactly one Review row per genuine review round.

## Acceptance Criteria

- [ ] `bin/e2e rust-version` passes the reviewer step: bundle reaches
      Reviewed/Accepted, integrator runs, ticks > 0.
- [ ] The 3 `failure_paths` tests (`crates/loopr/tests/failure_paths.rs:67,169,223`)
      pass, 3 consecutive runs.
- [ ] No raw `.bundles().update` call site remains in `crates/loopr/src/daemon/`
      outside the helper (`rg -U '\.bundles\(\)\s*\.update'`, multiline).
- [ ] A Stale write whose bundle never advanced emits an ERROR log in both arms
      (unit-tested, break-to-proven).
- [ ] `otto ci` green at repo root.

## Resolved Decisions

- 2026-07-12: F6 still-Triaged branch logs ERROR only, no Work->Blocked transition.
  Rationale: post-fix, reconcile re-spawn is self-healing (fresh read +
  Unchanged-skip); the branch should be unreachable and needs visibility, not new
  routing invented for a dead path. Both panel reviewers concur, conditional on the
  Phase 3 branch enumeration above (folded in).
- 2026-07-12: env-race test flake excluded as unrelated (separate mechanism,
  test-hygiene scope).
- 2026-07-12: Bundle-specific `BundleTransitionError` over reusing the Work-worded
  `TransitionError` (staff-engineer finding, accepted: names tell the truth).
- 2026-07-12: no status-preserving touch path. Unchanged-skip intentionally removes
  timestamp-only bumps; the unconditional touch was the doom mechanism. Future seam
  if ever needed: explicit `force_touch`. (Architect risk, demoted to a note by
  panel consensus; author concurs.)
- 2026-07-12: no telemetry counter on the invariant-violation branch. Intentional:
  a single ERROR log is sufficient for a branch the design makes unreachable.
  (Architect finding, demoted by panel consensus; author concurs.)
- 2026-07-12: panel closure round complete. Architect (Gemini) and Staff Engineer
  (Codex) confirm findings 1-5 and 7-10 closed against the folded-in text; F6
  ERROR-only routing consensus-resolved. Finding 6 (Review.round deferral) is
  technically agreed by both reviewers and waits only on the owner's deferral
  sign-off (see next entry).
- 2026-07-12: Scott signed off on deferring the `Review.round` allocation race
  (`reviewer.rs:272-273`). Not harmless if it ever fires: two `round=1` rows make
  `decide_accept` see `latest.round=1, count=2`, mismatch, fail closed
  (`review.rs:220+`), wedging a Reviewed bundle. Safe today only under the composed
  single-daemon / no-duplicate-spawn invariant (see Architecture). Revisit
  condition: any multi-reviewer, multi-daemon, or duplicate-spawn support must fix
  round allocation or accept-gate semantics first.

## Alternatives Considered

### Alternative 1: retry-on-stale in the reviewer
- **Description:** `run_reviewer` re-reads the bundle and retries the transition on
  `StoreError::Stale`.
- **Pros:** localizes the fix to the losing write; robust against any future stale
  source.
- **Cons:** masks the invariant break instead of fixing it; the daemon would still
  be poisoning its own copies; retry loops on OCC are the pattern `71746cdc`
  explicitly replaced with floor+return.
- **Why not chosen:** fixes the symptom at the wrong altitude. Prior art
  (`transition_and_persist_work`, integrator's `transition_bundle_returning`)
  already solves this class at the source.

### Alternative 2: store-level auto-sync (mutate the passed bundle in `update`)
- **Description:** `BundlesStore::update` takes `&mut Bundle` and writes the floored
  timestamp back itself.
- **Pros:** impossible to forget the re-sync.
- **Cons:** API change in the store crate; blast radius crosses a crate boundary for
  a bug that lives entirely in loopr; diverges Bundles from the Work/Plan store API
  shape (siblings behave identically).
- **Why not chosen:** cross-crate change for a single-crate bug; the helper gives
  the same can't-forget property inside the crate that owns the call sites.

### Alternative 3: uniqueness guard on (bundle_id, round)
- **Description:** dedupe Review rows to stop the duplicates.
- **Pros:** caps LLM spend if any future loop recurs.
- **Cons:** treats the symptom; duplicates only exist because the loop exists.
- **Why not chosen:** the loop is the bug. Rows are append-only by design.

## Technical Considerations

### Dependencies

- Internal only: crates/loopr (fix), crates/store (contract defended, unchanged),
  crates/agents + crates/domain + crates/integrator (read, unchanged).
- Blast radius: loopr crate only. No cross-repo impact; no ship-order constraint.

### Performance

- Unchanged-skip removes one redundant disk write per re-spawned triage. Otherwise
  neutral.

### Security

- None. No new inputs, no auth surface.

### Testing Strategy

- Seam test at the loopr/store boundary (the fix defends the store's OCC contract).
- Break-to-prove on every new test: demonstrated red against the pre-fix shape.
- Live e2e as the final gate (Phase 4), since the unit gap is exactly what let this
  ship red.

### Rollout Plan

- Single branch on v5, one commit per phase, `otto ci` green per phase. No
  coexistence: all raw call sites convert in the Phase 1 commit.
- Rollback: `git revert` the branch. No schema changes; a downgraded binary reads
  existing state safely.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Another hand-rolled `.bundles().update` site appears later | Med | High (same class) | grep acceptance criterion; helper is the only exported path in the daemon module |
| Unchanged-skip hides a legitimate need to touch `updated_at` | Low | Med | Phase 2 integration test covers the re-spawn path end-to-end; reconcile age clock is driven by disk state, which the skip leaves honest |
| Phase 4 blocked on API key availability | Med | Low | operator step, explicitly not a phase agent; run interactively |
| stage_8/stage_9 flake has a second cause | Low | Med | Phase 2 success criterion requires 3 consecutive green runs; if still flaky, that is a finding, not a pass |

## Open Questions

None.

## References

- docs/design/2026-07-11-verified-swarm-implementation-notes.md (line 18: suspected
  shared root cause)
- Commit `71746cdc` (2026-06-10): floor+return OCC remediation that missed the two
  Bundle sites
- Live repro: `~/.local/share/loopr/sessions/20260712-163709-2/targets/-tmp-loopr-e2e-rust-version-20260712-163709/runs/pc-qc23ip/events.log`, bundle `bd-cx1u1`
- Prior art: `transition_and_persist_work` (`crates/loopr/src/daemon/context.rs:1617-1623`),
  `integrator::transition_bundle_returning` (`crates/integrator/src/lib.rs:750-778`)

## Addendum: Implementation status (2026-07-12)

Point-in-time record of what actually landed vs. what the plan assumed. The
`failure_paths` goal turned out to be a multi-bug recovery-path chain, not the
single OCC fix this doc scoped; Scott's call was to consolidate the proven work
and take `failure_paths` as its own effort.

### Landed and verified

- **Phase 1 (OCC helper + call-site conversions): done, proven.** Live
  diagnostics confirmed the reviewer's `Triaged -> Reviewed/Rejected` write now
  succeeds on the first round (`OK floored=...`), not `Stale`. The reviewer runs
  exactly twice in the reject->accept scenario. The self-stale doom loop is gone.
- **Deviation (disclosed): the helper does NOT live in `context.rs`.** `context.rs`
  was already 1825 lines at HEAD, over the 1500 bloat gate before this change, so
  adding the helper worsened an already-red file. Per the in-house decomposition
  pattern (`spawner.rs`/`integration.rs`/`reap.rs`), the transition helpers moved
  to `crates/loopr/src/daemon/context/transition.rs` and the sibling-Work helpers
  to `context/siblings.rs`, both re-exported from `context` so every call site
  resolves unchanged. `context.rs` is now 1402 lines. `transport/handler/tests.rs`
  (1503, also pre-existing over) split its `director.chat` tests into
  `handler/tests/chat.rs`.
- **`director_chat::note_persists_across_daemon_restart`: pre-existing broken
  test, rewritten.** It was committed already-red (c67a8eb9), requiring a live
  authenticated Anthropic round-trip inside `otto ci` (impossible: placeholder
  key -> real 401 -> `Fatal(Auth)`). Rewritten to assert the credential-independent
  invariant the design actually claims -- the post-restart Director spawns and
  OBSERVES the seeded note in its first iteration (before the LLM call) -- and to
  seed a child Work so the Plan stays Active instead of going Stalled on boot.
  Green in ~5s.

### Latent bug found and fixed (separate crate)

- **Integrator working-tree-dirty guard tripped on loopr's own state.**
  `integrator::git::working_tree_dirty` ran a bare `git status --porcelain` and
  treated `?? .loopr/` (loopr's own `.loopr/taskstore/`, deliberately not in
  `ensure_loopr_excludes` because TaskStore is committed) as the operator's
  uncommitted work, refusing to integrate. Latent because every prior run wedged
  at the reviewer OCC race before the integrator ran; Phase 1 un-masked it. Fixed
  by scoping the guard to `git status --porcelain -- . ':(exclude).loopr/'` with
  break-to-prove tests. This is the fix that lets the recovery pipeline reach a
  Tick at all.

### Deferred to a separate effort: `failure_paths` green

The doc's Goal "the 3 red `failure_paths` tests go green" is NOT met by the OCC
fix alone. These tests never passed; they hid a chain of latent recovery-path
bugs, each masked by the one before:

1. Reviewer OCC self-stale -- **fixed** (Phase 1).
2. Integrator dirty-guard on `.loopr/` -- **fixed** (above).
3. **Open:** the reject->retry recovery persists **1 Bundle (Merged), not the
   expected 2 (Rejected + Merged)** -- `attempt_count=2` but the rejected
   attempt's Bundle record is absent. Undetermined whether this is a real
   lost-Bundle bug or a never-validated test expectation; likely more behind it.

Phases 2 (regression tests), 3 (loud-fail Stale discrimination), and 4 (live
e2e) remain unstarted. `stage_8`/`stage_9` flake is plausibly the same
integrator-dirty timing issue now fixed, but is unverified pending the separate
effort. That effort owns: root-causing item 3, completing Phases 2-4, and
correcting this doc's premise that the OCC fix was sufficient.
